//! Browser files embedded in the binary. No build step: `web/` is served as written.
//! ^ Only three documents are filled in at serve time, from the validated site file:
//! index.html (base path, name), the manifest and the service worker. The theme's
//! stylesheet is built in theme.rs.
use crate::{devices::hex, site::Site};
use serde_json::json;
use sha2::{Digest, Sha256};

const JS: &str = "text/javascript; charset=utf-8";
const FILES: &[(&str, &str, &str)] = &[
    (
        "terminal-fonts.css",
        "text/css; charset=utf-8",
        include_str!("../web/terminal-fonts.css"),
    ),
    (
        "app.css",
        "text/css; charset=utf-8",
        include_str!("../web/app.css"),
    ),
    ("main.js", JS, include_str!("../web/main.js")),
    ("lib.js", JS, include_str!("../web/lib.js")),
    ("store.js", JS, include_str!("../web/store.js")),
    ("api.js", JS, include_str!("../web/api.js")),
    ("dom.js", JS, include_str!("../web/dom.js")),
    ("ui.js", JS, include_str!("../web/ui.js")),
    ("pair.js", JS, include_str!("../web/pair.js")),
    ("inbox.js", JS, include_str!("../web/inbox.js")),
    ("home.js", JS, include_str!("../web/home.js")),
    ("projects.js", JS, include_str!("../web/projects.js")),
    ("session.js", JS, include_str!("../web/session.js")),
    ("chat.js", JS, include_str!("../web/chat.js")),
    ("tasks.js", JS, include_str!("../web/tasks.js")),
    ("task.js", JS, include_str!("../web/task.js")),
    ("story.js", JS, include_str!("../web/story.js")),
    ("checks.js", JS, include_str!("../web/checks.js")),
    ("bubbles.js", JS, include_str!("../web/bubbles.js")),
    ("reply.js", JS, include_str!("../web/reply.js")),
    ("page.js", JS, include_str!("../web/page.js")),
    ("markdown.js", JS, include_str!("../web/markdown.js")),
    ("diagram.js", JS, include_str!("../web/diagram.js")),
    ("files.js", JS, include_str!("../web/files.js")),
    ("settings.js", JS, include_str!("../web/settings.js")),
    ("terminal.js", JS, include_str!("../web/terminal.js")),
    // Pinned by scripts/vendor-preact.sh; licenses in web/vendor/preact.LICENSE and htm.LICENSE.
    ("preact.js", JS, include_str!("../web/vendor/preact.js")),
    (
        "preact-hooks.js",
        JS,
        include_str!("../web/vendor/preact-hooks.js"),
    ),
    ("htm.js", JS, include_str!("../web/vendor/htm.js")),
    // Pinned by scripts/vendor-markdown.sh; licenses in web/vendor/markdown-it.LICENSE and highlight.LICENSE.
    (
        "markdown-it.js",
        JS,
        include_str!("../web/vendor/markdown-it.js"),
    ),
    (
        "highlight.js",
        JS,
        include_str!("../web/vendor/highlight.js"),
    ),
    // Pinned and patched by scripts/vendor-xterm.sh; license in web/vendor/xterm.LICENSE.
    ("xterm.js", JS, include_str!("../web/vendor/xterm.js")),
    (
        "xterm-addon-fit.js",
        JS,
        include_str!("../web/vendor/addon-fit.js"),
    ),
    (
        "xterm.css",
        "text/css; charset=utf-8",
        include_str!("../web/vendor/xterm.css"),
    ),
];

// ^ docs/web-client-terminal.md "Terminal fonts": pinned by scripts/vendor-fonts.sh, licenses
// beside them. The names carry the upstream version; they are not part of the offline shell.
const WOFF2: &str = "font/woff2";
const FONTS: &[(&str, &str, &[u8])] = &[
    (
        "JetBrainsMono-2.304-Regular.woff2",
        WOFF2,
        include_bytes!("../web/vendor/fonts/JetBrainsMono-Regular.woff2"),
    ),
    (
        "JetBrainsMono-2.304-Bold.woff2",
        WOFF2,
        include_bytes!("../web/vendor/fonts/JetBrainsMono-Bold.woff2"),
    ),
    (
        "JetBrainsMono-2.304-Italic.woff2",
        WOFF2,
        include_bytes!("../web/vendor/fonts/JetBrainsMono-Italic.woff2"),
    ),
    (
        "SymbolsNerdFontMono-3.4.0.woff2",
        WOFF2,
        include_bytes!("../web/vendor/fonts/SymbolsNerdFontMono-Regular.woff2"),
    ),
    (
        "SymbolsNerdFontMono-3.4.0.ttf",
        "font/ttf",
        include_bytes!("../web/vendor/fonts/SymbolsNerdFontMono-Regular.ttf"),
    ),
];

// ^ docs/web-client-chat.md "Diagrams": pinned and patched by scripts/vendor-mermaid.sh, license
// in web/vendor/mermaid.LICENSE. Stored gzip-compressed and served that way. The page
// asks for it when a message holds a diagram; it is not part of the offline shell.
pub const MERMAID: &str = "mermaid-11.17.2.js";
const MERMAID_GZIP: &[u8] = include_bytes!("../web/vendor/mermaid.js.gz");

pub struct Asset {
    pub content_type: &'static str,
    pub body: &'static [u8],
    /// The name carries the version, so the response never changes.
    pub pinned: bool,
    /// The body is gzip-compressed and is sent as it is.
    pub gzip: bool,
}

pub fn find(name: &str) -> Option<Asset> {
    let asset = |content_type, body, pinned, gzip| Asset {
        content_type,
        body,
        pinned,
        gzip,
    };
    if name == MERMAID {
        return Some(asset(JS, MERMAID_GZIP, true, true));
    }
    let text = FILES
        .iter()
        .find(|(file, _, _)| *file == name)
        .map(|(_, content_type, body)| asset(content_type, body.as_bytes(), false, false));
    text.or_else(|| {
        FONTS
            .iter()
            .find(|(file, _, _)| *file == name)
            .map(|(_, content_type, body)| asset(content_type, body, true, false))
    })
}

pub struct Assets {
    pub index: String,
    pub manifest: String,
    pub service_worker: String,
    /// The digest of every shell byte. Module URLs carry it (`assets/<build>/main.js`), so
    /// no cache serves an old module beside a new one, and a page compares it with the
    /// server's to offer a reload (docs/web-client.md "Build").
    pub build: String,
}

/// The git commit the binary was built from, when it was built from git (build.rs).
pub const COMMIT: Option<&str> = option_env!("WSX_BUILD_COMMIT");

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

impl Assets {
    pub fn build(site: &Site) -> Self {
        let theme = &site.theme;
        let index = include_str!("../web/index.html")
            .replace("__WSX_BASE__", &site.base)
            .replace("__WSX_NAME__", &escape(&site.name))
            .replace("__WSX_THEME_CSS__", &format!("theme/{}", theme.stylesheet))
            .replace("__WSX_ICON__", &format!("theme/{}", theme.icon_192))
            .replace("__WSX_THEME_COLOR__", &theme.chrome_color)
            .replace("__WSX_THEME__", &escape(&theme.name))
            // A light-only theme must not get dark form controls from the browser.
            .replace(
                "__WSX_SCHEMES__",
                if theme.dark { "light dark" } else { "light" },
            );
        let scope = site.scope();
        let manifest = json!({
            "id": scope, "scope": scope, "start_url": scope,
            "name": site.name, "short_name": site.name,
            "display": "standalone",
            "background_color": theme.background_color,
            "theme_color": theme.chrome_color,
            // ^ Icons are full-bleed with the mark in the central 80 %, so a launcher may mask them
            // (docs/web-themes.md "Files").
            "icons": [
                { "src": format!("theme/{}", theme.icon_192), "sizes": "192x192", "type": "image/png", "purpose": "any maskable" },
                { "src": format!("theme/{}", theme.icon_512), "sizes": "512x512", "type": "image/png", "purpose": "any maskable" },
            ],
        })
        .to_string();
        // The cache name and the module path change whenever any shell byte changes, so
        // an old cache is dropped. Theme files are named by their content hash, so the
        // index covers them. The index is digested before the build is written into it.
        let mut digest = Sha256::new();
        for part in [&index, &manifest] {
            digest.update(part.as_bytes());
        }
        for (_, _, body) in FILES {
            digest.update(body.as_bytes());
        }
        let build = hex(&digest.finalize()[..6]);
        let index = index.replace("__WSX_BUILD__", &build);
        // Offline shell: the page, the manifest, the scripts, and every theme file
        // except the large icon.
        let mut shell = vec!["./".to_owned(), "manifest.webmanifest".to_owned()];
        shell.extend(
            theme
                .files
                .iter()
                .filter(|file| file.name != theme.icon_512)
                .map(|file| format!("theme/{}", file.name)),
        );
        shell.extend(
            FILES
                .iter()
                .map(|(name, _, _)| format!("assets/{build}/{name}")),
        );
        let service_worker = include_str!("../web/sw.js")
            .replace("__WSX_CACHE__", &format!("wsx-{}-{build}", site.id))
            .replace("__WSX_SHELL__", &json!(shell).to_string())
            .replace(
                "__WSX_LAZY__",
                &json!([format!("assets/{build}/{MERMAID}")]).to_string(),
            )
            .replace("__WSX_ICON__", &format!("theme/{}", theme.icon_192));
        Self {
            index,
            manifest,
            service_worker,
            build,
        }
    }
}
