//! Themes: validated tokens, optional component CSS, fonts and icons.
//! Contract: docs/web-themes.md.
//! ^ A theme is data from the deployer, but its output is injected into a page
//! under a strict CSP. Every token value is checked against the grammar of its
//! kind, and component CSS may not reference any URL, so a theme cannot load a
//! remote resource or break out of a declaration.
use crate::{devices::hex, sandbox::Root};
use anyhow::{bail, ensure, Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, path::Path};

const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
const MAX_COMPONENT_BYTES: u64 = 128 * 1024;
const MAX_FONT_BYTES: u64 = 2 * 1024 * 1024;
const MAX_ICON_BYTES: u64 = 1024 * 1024;
const MAX_FONTS: usize = 8;

type Files = &'static [(&'static str, &'static [u8])];
const BUILTIN: &[(&str, Files)] = &[(
    "default",
    &[
        (
            "theme.toml",
            include_bytes!("../web/themes/default/theme.toml"),
        ),
        (
            "icon-192.png",
            include_bytes!("../web/themes/default/icon-192.png"),
        ),
        (
            "icon-512.png",
            include_bytes!("../web/themes/default/icon-512.png"),
        ),
    ],
)];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    name: String,
    #[serde(default)]
    extends: Option<String>,
    #[serde(default)]
    components: Option<String>,
    #[serde(default)]
    tokens: BTreeMap<String, String>,
    #[serde(default)]
    tokens_dark: Option<BTreeMap<String, String>>,
    #[serde(default)]
    fonts: Vec<FontFile>,
    #[serde(default)]
    icons: Option<IconFiles>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FontFile {
    family: String,
    file: String,
    #[serde(default = "normal_weight")]
    weight: String,
    #[serde(default = "normal_style")]
    style: String,
}
fn normal_weight() -> String {
    "400".into()
}
fn normal_style() -> String {
    "normal".into()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IconFiles {
    png_192: String,
    png_512: String,
}

/// Where a theme's files are read from: the binary, or a pinned directory that
/// is read without following symlinks.
enum Source {
    Builtin(Files),
    Directory(Root),
}

impl Source {
    fn read(&self, relative: &str, limit: u64) -> Result<Vec<u8>> {
        match self {
            Self::Builtin(files) => files
                .iter()
                .find(|(name, _)| *name == relative)
                .map(|(_, bytes)| bytes.to_vec())
                .with_context(|| format!("built-in theme file {relative} is missing")),
            Self::Directory(root) => root.read(Path::new(relative), limit).map_err(|error| {
                anyhow::anyhow!(
                    "theme file {relative}: {}",
                    match error {
                        crate::sandbox::SandboxError::TooLarge =>
                            format!("larger than {limit} bytes"),
                        crate::sandbox::SandboxError::NotFound => "not found".into(),
                        _ => "must be a regular file inside the theme directory".into(),
                    }
                )
            }),
        }
    }
}

/// One served theme file: `theme/<name>` with an immutable cache lifetime.
pub struct ThemeFile {
    pub name: String,
    pub content_type: &'static str,
    pub bytes: Vec<u8>,
}

pub struct Theme {
    pub name: String,
    /// False when the theme defines the light scheme only.
    pub dark: bool,
    /// Stylesheet, then fonts, then the 192 and 512 px icons.
    pub files: Vec<ThemeFile>,
    pub stylesheet: String,
    pub icon_192: String,
    pub icon_512: String,
    /// PWA manifest colors, taken from the theme's own tokens.
    pub chrome_color: String,
    pub background_color: String,
}

#[derive(Default)]
struct Resolved {
    tokens: BTreeMap<String, String>,
    dark: Option<BTreeMap<String, String>>,
    components: String,
    fonts: Vec<(FontFile, &'static str, Vec<u8>)>,
    icons: Option<(Vec<u8>, Vec<u8>)>,
}

impl Theme {
    /// `spec` is a built-in name, a theme directory, or a theme `.toml` file.
    /// `site_icons` from the site file replace the theme's icons.
    pub fn load(spec: &str, site_icons: Option<(Vec<u8>, Vec<u8>)>) -> Result<Self> {
        let mut resolved = Resolved::default();
        let name = if let Some((name, _)) = BUILTIN.iter().find(|(name, _)| *name == spec) {
            layer(name, &mut resolved)?;
            (*name).to_owned()
        } else {
            let path = Path::new(spec);
            ensure!(
                path.is_absolute(),
                "theme {spec:?} is not the built-in theme (default) or an absolute path"
            );
            let path = std::fs::canonicalize(path)
                .with_context(|| format!("theme {spec:?} does not exist"))?;
            let (directory, manifest) = if path.is_dir() {
                (path.as_path(), "theme.toml".to_owned())
            } else {
                (
                    path.parent().context("theme file has no directory")?,
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .context("theme file name is not UTF-8")?
                        .to_owned(),
                )
            };
            let source = Source::Directory(Root::pin(directory)?);
            custom(&source, &manifest, &mut resolved)?
        };
        let reference = reference_tokens();
        for token in reference.keys() {
            ensure!(
                resolved.tokens.contains_key(token),
                "theme {name} does not define token {token}"
            );
        }
        let icons = site_icons
            .or(resolved.icons.take())
            .context("theme has no icons")?;
        Ok(Self::assemble(name, resolved, icons))
    }

    fn assemble(name: String, resolved: Resolved, icons: (Vec<u8>, Vec<u8>)) -> Self {
        let named = |bytes: &[u8], extension: &str| {
            format!("{}.{extension}", hex(&Sha256::digest(bytes)[..8]))
        };
        let block = |tokens: &BTreeMap<String, String>| {
            tokens
                .iter()
                .map(|(token, value)| format!("--{token}:{value};"))
                .collect::<String>()
        };
        let mut files = Vec::new();
        let mut css = String::new();
        for (font, extension, bytes) in resolved.fonts {
            let file = named(&bytes, extension);
            // The stylesheet and the fonts share the `theme/` directory.
            css.push_str(&format!(
                "@font-face{{font-family:\"{}\";src:url(\"{file}\");font-weight:{};font-style:{};font-display:swap}}",
                font.family, font.weight, font.style
            ));
            files.push(ThemeFile {
                name: file,
                content_type: match extension {
                    "woff2" => "font/woff2",
                    "woff" => "font/woff",
                    "otf" => "font/otf",
                    _ => "font/ttf",
                },
                bytes,
            });
        }
        css.push_str(&format!(
            ":root{{color-scheme:light;{}}}",
            block(&resolved.tokens)
        ));
        // ^ docs/web-themes.md "Dark scheme rule": a theme with a dark scheme is dark
        // unless the device chose light. `data-scheme="dark"` on an element makes that
        // part dark in either case; the terminal frame uses it.
        if let Some(dark) = &resolved.dark {
            css.push_str(&format!(
                ":root:not([data-scheme=\"light\"]),[data-scheme=\"dark\"]{{color-scheme:dark;{}}}",
                block(dark)
            ));
        }
        css.push_str(&resolved.components);
        let scheme_color = resolved
            .dark
            .as_ref()
            .and_then(|dark| dark.get("color-bg"))
            .unwrap_or(&resolved.tokens["color-bg"])
            .clone();
        let stylesheet = named(css.as_bytes(), "css");
        let (icon_192, icon_512) = (named(&icons.0, "png"), named(&icons.1, "png"));
        files.insert(
            0,
            ThemeFile {
                name: stylesheet.clone(),
                content_type: "text/css; charset=utf-8",
                bytes: css.into_bytes(),
            },
        );
        for (file, bytes) in [(&icon_192, icons.0), (&icon_512, icons.1)] {
            files.push(ThemeFile {
                name: file.clone(),
                content_type: "image/png",
                bytes,
            });
        }
        Self {
            name,
            dark: resolved.dark.is_some(),
            stylesheet,
            icon_192,
            icon_512,
            // The page is dark when the theme has a dark scheme, so the installed
            // app's splash and toolbar take the dark background too.
            chrome_color: scheme_color.clone(),
            background_color: scheme_color,
            files,
        }
    }
}

/// The default theme's token names are the token contract.
fn reference_tokens() -> BTreeMap<String, String> {
    let text = std::str::from_utf8(BUILTIN[0].1[0].1).expect("built-in theme is UTF-8");
    toml::from_str::<Manifest>(text)
        .expect("built-in default theme parses")
        .tokens
}

/// Apply a built-in theme and, first, the built-in theme it extends.
fn layer(name: &str, resolved: &mut Resolved) -> Result<()> {
    let files = BUILTIN
        .iter()
        .find(|(candidate, _)| *candidate == name)
        .map(|(_, files)| *files)
        .with_context(|| format!("{name:?} is not the built-in theme (default)"))?;
    let source = Source::Builtin(files);
    let manifest = manifest(&source, "theme.toml")?;
    if let Some(parent) = &manifest.extends {
        ensure!(parent != name, "theme {name} extends itself");
        layer(parent, resolved)?;
    }
    apply(&source, manifest, resolved)
}

fn custom(source: &Source, file: &str, resolved: &mut Resolved) -> Result<String> {
    let manifest = manifest(source, file)?;
    ensure!(
        !manifest.name.is_empty()
            && manifest.name.len() <= 32
            && manifest
                .name
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'),
        "theme name must be 1-32 characters of a-z, 0-9 or '-'"
    );
    // A custom theme always sits on a built-in one, so it may define a few tokens only.
    layer(manifest.extends.as_deref().unwrap_or("default"), resolved)?;
    let name = manifest.name.clone();
    apply(source, manifest, resolved)?;
    Ok(name)
}

fn manifest(source: &Source, file: &str) -> Result<Manifest> {
    let bytes = source.read(file, MAX_MANIFEST_BYTES)?;
    toml::from_str(std::str::from_utf8(&bytes).context("theme file is not UTF-8")?)
        .with_context(|| format!("invalid theme file {file}"))
}

fn apply(source: &Source, manifest: Manifest, resolved: &mut Resolved) -> Result<()> {
    let reference = reference_tokens();
    for (token, value) in &manifest.tokens {
        ensure!(
            reference.contains_key(token),
            "{token} is not a theme token"
        );
        check_token(token, value)?;
    }
    // A layer that recolors the light scheme without a dark table has no dark
    // scheme: the inherited dark colors would not belong to it.
    let recolors = manifest
        .tokens
        .keys()
        .any(|token| token.starts_with("color-"));
    match manifest.tokens_dark {
        Some(dark) => {
            for (token, value) in &dark {
                ensure!(
                    reference.contains_key(token)
                        && ["color-", "opacity-"]
                            .iter()
                            .any(|kind| token.starts_with(kind)),
                    "tokens_dark.{token} must be a color or opacity token"
                );
                check_token(token, value)?;
            }
            resolved.dark.get_or_insert_with(BTreeMap::new).extend(dark);
        }
        None if recolors => resolved.dark = None,
        None => {}
    }
    resolved.tokens.extend(manifest.tokens);
    if let Some(file) = &manifest.components {
        let bytes = source.read(file, MAX_COMPONENT_BYTES)?;
        let css = String::from_utf8(bytes).context("component CSS is not UTF-8")?;
        check_components(&css)?;
        resolved.components.push_str(&css);
    }
    for font in manifest.fonts {
        ensure!(
            resolved.fonts.len() < MAX_FONTS,
            "a theme may ship at most {MAX_FONTS} fonts"
        );
        ensure!(
            !font.family.is_empty()
                && font.family.len() <= 64
                && font
                    .family
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '_')),
            "font family {:?} may use letters, digits, space, '-' and '_'",
            font.family
        );
        ensure!(
            matches!(font.weight.as_str(), "normal" | "bold")
                || font
                    .weight
                    .parse::<u16>()
                    .is_ok_and(|weight| (100..=900).contains(&weight)),
            "font weight must be 100-900, normal or bold"
        );
        ensure!(
            matches!(font.style.as_str(), "normal" | "italic"),
            "font style must be normal or italic"
        );
        let bytes = source.read(&font.file, MAX_FONT_BYTES)?;
        // The format comes from the file's signature, never from its name.
        let extension = match bytes.get(..4) {
            Some(b"wOF2") => "woff2",
            Some(b"wOFF") => "woff",
            Some(b"OTTO") => "otf",
            Some([0, 1, 0, 0]) => "ttf",
            _ => bail!("font {} is not WOFF2, WOFF, OTF or TTF", font.file),
        };
        resolved.fonts.push((font, extension, bytes));
    }
    if let Some(icons) = manifest.icons {
        let small = source.read(&icons.png_192, MAX_ICON_BYTES)?;
        let large = source.read(&icons.png_512, MAX_ICON_BYTES)?;
        check_png(&small, 192)?;
        check_png(&large, 512)?;
        resolved.icons = Some((small, large));
    }
    Ok(())
}

pub fn check_png(bytes: &[u8], size: u32) -> Result<()> {
    ensure!(
        bytes.len() as u64 <= MAX_ICON_BYTES
            && wsx_core::task::checks::png_size(bytes) == Some((size, size)),
        "icon must be a {size}x{size} PNG of at most 1 MiB"
    );
    Ok(())
}

fn number(text: &str) -> Option<f64> {
    let digits = text.strip_prefix('-').unwrap_or(text);
    (!digits.is_empty()
        && digits.len() <= 8
        && digits
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'.')
        && digits.bytes().filter(|byte| *byte == b'.').count() <= 1)
        .then(|| text.parse().ok())
        .flatten()
}

fn length(text: &str) -> bool {
    text == "0"
        || ["px", "rem", "em", "ch", "vh", "vw", "%"]
            .iter()
            .any(|unit| {
                text.strip_suffix(unit)
                    .is_some_and(|value| number(value).is_some())
            })
}

fn color(text: &str) -> bool {
    text.strip_prefix('#').is_some_and(|digits| {
        matches!(digits.len(), 3 | 6 | 8) && digits.bytes().all(|byte| byte.is_ascii_hexdigit())
    })
}

fn check_token(token: &str, value: &str) -> Result<()> {
    let valid = match token {
        _ if token.starts_with("color-") => color(value),
        "font-body" | "font-mono" | "font-terminal" => {
            !value.is_empty()
                && value.len() <= 200
                && value
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || " ,'-_".contains(c))
                && value.matches('\'').count().is_multiple_of(2)
        }
        _ if token.starts_with("font-weight-") => value
            .parse::<u16>()
            .is_ok_and(|weight| (100..=900).contains(&weight)),
        _ if token.starts_with("line-height") => {
            number(value).is_some_and(|height| (1.0..=3.0).contains(&height))
        }
        "motion-duration" => ["ms", "s"].iter().any(|unit| {
            value
                .strip_suffix(unit)
                .and_then(number)
                .is_some_and(|time| time >= 0.0)
        }),
        "motion-easing" => matches!(
            value,
            "linear" | "ease" | "ease-in" | "ease-out" | "ease-in-out"
        ),
        _ if token.starts_with("opacity-") => {
            number(value).is_some_and(|opacity| (0.0..=1.0).contains(&opacity))
        }
        _ if token.starts_with("tracking-") => length(value.trim_start_matches('-')),
        // font-size-*, space-*, size-*, radius-*, border-width, focus-width
        _ => length(value) && !value.starts_with('-'),
    };
    ensure!(valid, "token {token} has an invalid value {value:?}");
    Ok(())
}

/// Component CSS may restyle hooks but may not fetch anything or declare fonts.
fn check_components(css: &str) -> Result<()> {
    let lower = css.to_ascii_lowercase();
    for banned in [
        "url(",
        "@import",
        "@font-face",
        "image-set(",
        "src(",
        "expression(",
        "javascript:",
        "//",
        "</",
        "\\",
    ] {
        ensure!(
            !lower.contains(banned),
            "component CSS must not contain {banned:?}; declare fonts in the theme file"
        );
    }
    ensure!(!css.contains('\0'), "component CSS contains a NUL byte");
    Ok(())
}
