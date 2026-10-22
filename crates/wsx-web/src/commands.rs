//! The slash commands a session's agent takes, for the chat's `/` list
//! (docs/web-client-chat.md "Commands"). Names and one-line descriptions only: a file
//! body never leaves this module.
//! ^ Claude's folders and plugin layout: https://code.claude.com/docs/en/skills and
//! https://code.claude.com/docs/en/commands. Codex: developers.openai.com/codex/cli/slash-commands.
//! Pi 1.1.0: docs/slash-commands.md of @earendil-works/pi-coding-agent.
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::HashSet,
    fs,
    io::Read,
    path::{Path, PathBuf},
};

const MAX_COMMANDS: usize = 200;
const MAX_ENTRIES_PER_FOLDER: usize = 512;
const MAX_PLUGINS: usize = 64;
const MAX_HEAD_BYTES: u64 = 8 * 1024;
const MAX_JSON_BYTES: u64 = 256 * 1024;
const MAX_NAME_BYTES: usize = 64;
const MAX_DESCRIPTION_CHARS: usize = 160;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Builtin,
    Project,
    User,
    Plugin,
}

#[derive(Debug, Serialize)]
pub struct Command {
    pub name: String,
    pub description: String,
    pub source: Source,
}

#[derive(Debug, Serialize)]
pub struct Listing {
    /// False when only the provider's built-in commands are known.
    pub listed: bool,
    pub commands: Vec<Command>,
    /// More commands exist than `MAX_COMMANDS`.
    pub truncated: bool,
    /// Files that could not be read as a skill or a command.
    pub skipped: usize,
}

impl Listing {
    /// Whether a one-line message starts with one of these commands.
    pub fn has_command(&self, text: &str) -> bool {
        command_name(text).is_some_and(|name| {
            self.commands
                .iter()
                .any(|command| command.name.eq_ignore_ascii_case(name))
        })
    }
}

/// A one-line `!` shell line (`! git status`) for an agent that runs one from the start of
/// its input: Claude's shell mode (https://code.claude.com/docs/en/interactive-mode).
/// ^ Pi's `!` and `!!` start no turn, so wsxd would see no end of the round; Codex
/// documents none.
pub fn is_shell_line(provider: &str, text: &str) -> bool {
    provider == "claude"
        && text.len() <= MAX_SHELL_LINE_BYTES
        && !text.contains(['\n', '\t'])
        && text
            .strip_prefix('!')
            .is_some_and(|rest| !rest.trim().is_empty())
}

/// wsxd's bound of a command round (docs/agent-orchestration.md "Commands").
const MAX_SHELL_LINE_BYTES: usize = 256;

/// `/name` or `/name arguments` on one line, without the slash.
fn command_name(text: &str) -> Option<&str> {
    let rest = text.strip_prefix('/')?;
    if text.contains('\n') {
        return None;
    }
    rest.split_whitespace()
        .next()
        .filter(|name| valid_name(name))
}

fn builtins(provider: &str) -> &'static [(&'static str, &'static str)] {
    match provider {
        "claude" => &[
            ("compact", "Summarize the conversation to free up context"),
            ("clear", "Start a new conversation with empty context"),
        ],
        "codex" => &[
            ("compact", "Summarize the visible chat to free tokens"),
            ("new", "Start a new chat inside the same session"),
        ],
        "pi" => &[
            ("compact", "Compact the current context"),
            ("new", "Start a new session"),
        ],
        _ => &[],
    }
}

/// The commands of `provider` in a session whose worktree is `worktree`.
pub fn list(provider: &str, worktree: &Path) -> Listing {
    let mut listing = Listing {
        listed: provider == "claude",
        commands: Vec::new(),
        truncated: false,
        skipped: 0,
    };
    let mut seen = HashSet::new();
    let mut add = |listing: &mut Listing, name: String, description: String, source| {
        if !seen.insert(name.to_ascii_lowercase()) {
            return;
        }
        if listing.commands.len() == MAX_COMMANDS {
            listing.truncated = true;
            return;
        }
        listing.commands.push(Command {
            name,
            description,
            source,
        });
    };
    for (name, description) in builtins(provider) {
        add(
            &mut listing,
            (*name).into(),
            (*description).into(),
            Source::Builtin,
        );
    }
    if provider != "claude" {
        return listing;
    }
    let home = claude_home();
    let mut found = Vec::new();
    let project = worktree.join(".claude");
    scan(
        &project,
        "",
        Source::Project,
        &mut found,
        &mut listing.skipped,
    );
    if let Some(home) = &home {
        scan(home, "", Source::User, &mut found, &mut listing.skipped);
        for (plugin, path) in plugins(home, worktree) {
            scan(
                &path,
                &format!("{plugin}:"),
                Source::Plugin,
                &mut found,
                &mut listing.skipped,
            );
        }
    }
    // Project first, so a project's command wins over a user's of the same name.
    for (name, description, source) in found {
        add(&mut listing, name, description, source);
    }
    listing
}

/// `$CLAUDE_CONFIG_DIR`, else `~/.claude`.
fn claude_home() -> Option<PathBuf> {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".claude")))
}

/// `skills/<name>/SKILL.md` and `commands/[<folder>/]<name>.md` under `root`.
fn scan(
    root: &Path,
    prefix: &str,
    source: Source,
    found: &mut Vec<(String, String, Source)>,
    skipped: &mut usize,
) {
    // ^ A repository's own `.claude` may hold symlinks to any file of the user, and a listed
    // description goes to paired devices: there only real files and folders count. The
    // user's and the plugins' folders are the user's own and often linked.
    let real = source == Source::Project;
    let is = |path: &Path, dir: bool| {
        let meta = if real {
            fs::symlink_metadata(path)
        } else {
            fs::metadata(path)
        };
        meta.is_ok_and(|meta| if dir { meta.is_dir() } else { meta.is_file() })
    };
    if real && !is(root, true) {
        return;
    }
    let skills = root.join("skills");
    let skill_entries = match !real || is(&skills, true) {
        true => entries(&skills),
        false => Vec::new(),
    };
    for (stem, path) in skill_entries {
        let file = path.join("SKILL.md");
        if !is(&path, true) || !is(&file, false) {
            continue;
        }
        match read_head(&file).and_then(|head| parse(&head, &stem)) {
            Some((name, description)) => {
                found.push((format!("{prefix}{name}"), description, source))
            }
            None => *skipped += 1,
        }
    }
    let commands = root.join("commands");
    if real && !is(&commands, true) {
        return;
    }
    for (stem, path) in entries(&commands) {
        if is(&path, true) {
            // One folder level: `commands/frontend/component.md` is `frontend:component`.
            for (inner, file) in entries(&path)
                .into_iter()
                .filter(|(_, file)| is(file, false))
            {
                command_file(
                    &file,
                    &format!("{prefix}{stem}:"),
                    &inner,
                    source,
                    found,
                    skipped,
                );
            }
        } else if is(&path, false) {
            command_file(&path, prefix, &stem, source, found, skipped);
        }
    }
}

fn command_file(
    file: &Path,
    prefix: &str,
    stem: &str,
    source: Source,
    found: &mut Vec<(String, String, Source)>,
    skipped: &mut usize,
) {
    let Some(stem) = stem.strip_suffix(".md") else {
        return;
    };
    // A command's name is its file name; frontmatter `name` is a skill field only.
    match read_head(file).and_then(|head| parse(&head, stem)) {
        Some((_, description)) if valid_name(stem) => {
            found.push((format!("{prefix}{stem}"), description, source))
        }
        _ => *skipped += 1,
    }
}

/// The names and paths of a folder, sorted, at most `MAX_ENTRIES_PER_FOLDER`, hidden ones left out.
fn entries(folder: &Path) -> Vec<(String, PathBuf)> {
    let Ok(read) = fs::read_dir(folder) else {
        return Vec::new();
    };
    let mut entries: Vec<_> = read
        .flatten()
        .take(MAX_ENTRIES_PER_FOLDER)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            (!name.starts_with('.')).then(|| (name, entry.path()))
        })
        .collect();
    entries.sort();
    entries
}

/// The first `MAX_HEAD_BYTES` of a file, as text. A cut multi-byte character at the end is dropped.
fn read_head(file: &Path) -> Option<String> {
    let mut bytes = Vec::new();
    fs::File::open(file)
        .ok()?
        .take(MAX_HEAD_BYTES)
        .read_to_end(&mut bytes)
        .ok()?;
    match String::from_utf8(bytes) {
        Ok(text) => Some(text),
        Err(error) if error.utf8_error().error_len().is_none() => {
            let valid = error.utf8_error().valid_up_to();
            let mut bytes = error.into_bytes();
            bytes.truncate(valid);
            String::from_utf8(bytes).ok()
        }
        Err(_) => None,
    }
}

/// `name` and a one-line `description` from YAML frontmatter, else the default name and
/// the first line of the text. An unterminated frontmatter or an invalid name is `None`.
fn parse(text: &str, default_name: &str) -> Option<(String, String)> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let (front, body) = match text.strip_prefix("---") {
        Some(rest) if rest.starts_with(['\n', '\r']) => {
            let end = rest.find("\n---")?;
            let after = &rest[end + 4..];
            (
                Some(&rest[..end]),
                after.split_once('\n').map_or("", |(_, body)| body),
            )
        }
        _ => (None, text),
    };
    let mut name = None;
    let mut description = None;
    if let Some(front) = front {
        let lines: Vec<&str> = front.lines().collect();
        for (index, line) in lines.iter().enumerate() {
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            if line.starts_with([' ', '\t']) {
                continue;
            }
            let value = value.trim();
            let value = if matches!(value, "" | "|" | ">" | "|-" | ">-") {
                // A block value: the indented lines that follow.
                lines[index + 1..]
                    .iter()
                    .take_while(|next| next.starts_with([' ', '\t']) || next.trim().is_empty())
                    .map(|next| next.trim())
                    .filter(|next| !next.is_empty())
                    .collect::<Vec<_>>()
                    .join(" ")
            } else {
                unquote(value).to_owned()
            };
            match key.trim() {
                "name" => name = Some(value),
                "description" => description = Some(value),
                _ => {}
            }
        }
    }
    let name = name
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| default_name.to_owned());
    if !valid_name(&name) {
        return None;
    }
    let description = description
        .filter(|text| !text.is_empty())
        .or_else(|| {
            body.lines()
                .map(|line| line.trim().trim_start_matches('#').trim())
                .find(|line| !line.is_empty())
                .map(str::to_owned)
        })
        .unwrap_or_default();
    Some((name, one_line(&description)))
}

fn unquote(value: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(inner) = value
            .strip_prefix(quote)
            .and_then(|rest| rest.strip_suffix(quote))
        {
            return inner;
        }
    }
    value
}

/// Control characters removed, whitespace folded, at most `MAX_DESCRIPTION_CHARS` characters.
fn one_line(text: &str) -> String {
    let folded = text
        .split(|character: char| character.is_whitespace() || character.is_control())
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if folded.chars().count() <= MAX_DESCRIPTION_CHARS {
        return folded;
    }
    let mut cut: String = folded.chars().take(MAX_DESCRIPTION_CHARS - 1).collect();
    cut.push('…');
    cut
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_BYTES
        && name.starts_with(|character: char| character.is_ascii_alphanumeric())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
}

/// Enabled plugins with an install that applies to `worktree`: user installs, and
/// project or local installs of this worktree. Named by the part before `@`.
fn plugins(home: &Path, worktree: &Path) -> Vec<(String, PathBuf)> {
    let read = |path: PathBuf| -> Option<Value> {
        let file = fs::File::open(path).ok()?;
        let mut text = String::new();
        file.take(MAX_JSON_BYTES).read_to_string(&mut text).ok()?;
        serde_json::from_str(&text).ok()
    };
    // The user's settings, then the project's, then the local ones: the last that names a plugin decides.
    let settings: Vec<Value> = [
        home.join("settings.json"),
        worktree.join(".claude/settings.json"),
        worktree.join(".claude/settings.local.json"),
    ]
    .into_iter()
    .filter_map(read)
    .collect();
    let enabled = |key: &str| {
        settings
            .iter()
            .rev()
            .find_map(|settings| settings.get("enabledPlugins")?.get(key)?.as_bool())
            == Some(true)
    };
    let Some(installed) = read(home.join("plugins/installed_plugins.json")) else {
        return Vec::new();
    };
    let Some(map) = installed.get("plugins").and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut plugins = Vec::new();
    for (key, installs) in map.iter().take(MAX_PLUGINS) {
        let plugin = key.split('@').next().unwrap_or(key);
        if !valid_name(plugin) || !enabled(key) {
            continue;
        }
        let applies = |install: &&Value| match install.get("scope").and_then(Value::as_str) {
            Some("user") | None => true,
            Some(_) => install
                .get("projectPath")
                .and_then(Value::as_str)
                .is_some_and(|path| Path::new(path) == worktree),
        };
        if let Some(path) = installs
            .as_array()
            .into_iter()
            .flatten()
            .filter(applies)
            .find_map(|install| install.get("installPath")?.as_str())
        {
            plugins.push((plugin.to_owned(), PathBuf::from(path)));
        }
    }
    plugins
}
