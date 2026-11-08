//! Project panels: a command in `wsx.config.yml` whose output the web client and the TUI show.
//! wsxd runs it and keeps the last result; the clients only ask (docs/panels.md).

use crate::process::{run_bounded, Bound, Options, Stderr};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

const MAX_PANELS: usize = 16;
pub const MAX_NAME_BYTES: usize = 32;
const MIN_REFRESH_SECS: u64 = 10;
const DEFAULT_TIMEOUT_SECS: u64 = 30;
const MAX_TIMEOUT_SECS: u64 = 300;
/// What a result keeps of the output: its last bytes.
pub const MAX_OUTPUT_BYTES: usize = 16 * 1024;
/// What a `format: json` document keeps (docs/panels.md "JSON panels").
const MAX_SECTIONS: usize = 12;
const MAX_ITEMS: usize = 50;
const MAX_COLUMNS: usize = 8;
const MAX_ROWS: usize = 100;
const MAX_STRING_CHARS: usize = 200;
/// Notice lines shown before "and n more".
const MAX_NOTICES: usize = 5;
const MAX_ARGV: usize = 64;
const MAX_ARG_BYTES: usize = 4096;

/// One `panels:` entry, checked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanelConfig {
    pub name: String,
    pub command: Vec<String>,
    /// Seconds between runs while a client shows the panel; 0 runs it only on open.
    pub refresh: u64,
    /// Seconds before the command is stopped.
    pub timeout: u64,
    pub format: PanelFormat,
    pub scope: PanelScope,
}

/// Whose state a panel shows: one project's, or the machine's (the web's Machine tab, the TUI's `M`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PanelScope {
    #[default]
    Project,
    Machine,
}

/// How a panel's output is read: as text, or as a JSON document drawn as a UI.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PanelFormat {
    #[default]
    Text,
    Json,
}

/// One `panels:` entry as written.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct PanelFile {
    name: String,
    command: Vec<String>,
    refresh: u64,
    timeout: Option<u64>,
    format: Option<String>,
    scope: Option<String>,
}

/// The `panels:` list checked: the error names the entry and the rule it breaks.
pub(crate) fn validate(files: Vec<PanelFile>) -> Result<Vec<PanelConfig>, String> {
    if files.len() > MAX_PANELS {
        return Err(format!("panels: at most {MAX_PANELS} panels"));
    }
    let mut panels: Vec<PanelConfig> = Vec::new();
    for (index, file) in files.into_iter().enumerate() {
        let at = format!("panels[{index}]");
        let name = file.name;
        if name.is_empty()
            || name.len() > MAX_NAME_BYTES
            || !name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
        {
            return Err(format!(
                "{at}.name must be 1..={MAX_NAME_BYTES} of a-z 0-9 - _, not {name:?}"
            ));
        }
        if panels.iter().any(|panel| panel.name == name) {
            return Err(format!("{at}.name {name:?} is used twice"));
        }
        if file.command.is_empty()
            || file.command.len() > MAX_ARGV
            || file.command[0].is_empty()
            || file.command.iter().any(|arg| arg.len() > MAX_ARG_BYTES)
        {
            return Err(format!(
                "{at}.command must be a program and up to {} arguments, each at most {MAX_ARG_BYTES} bytes",
                MAX_ARGV - 1
            ));
        }
        if file.refresh != 0 && file.refresh < MIN_REFRESH_SECS {
            return Err(format!(
                "{at}.refresh must be 0 or at least {MIN_REFRESH_SECS} seconds"
            ));
        }
        let timeout = file.timeout.unwrap_or(DEFAULT_TIMEOUT_SECS);
        if !(1..=MAX_TIMEOUT_SECS).contains(&timeout) {
            return Err(format!(
                "{at}.timeout must be 1..={MAX_TIMEOUT_SECS} seconds"
            ));
        }
        let format = match file.format.as_deref() {
            None | Some("text") => PanelFormat::Text,
            Some("json") => PanelFormat::Json,
            Some(other) => {
                return Err(format!("{at}.format must be text or json, not {other:?}"));
            }
        };
        let scope = match file.scope.as_deref() {
            None | Some("project") => PanelScope::Project,
            Some("machine") => PanelScope::Machine,
            Some(other) => {
                return Err(format!(
                    "{at}.scope must be project or machine, not {other:?}"
                ));
            }
        };
        panels.push(PanelConfig {
            name,
            command: file.command,
            refresh: file.refresh,
            timeout,
            format,
            scope,
        });
    }
    Ok(panels)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PanelStatus {
    /// No run has finished yet.
    Pending,
    /// Exit 0.
    Ok,
    /// A non-zero exit, or a signal.
    Failed,
    /// Stopped at its timeout.
    TimedOut,
    /// The command could not start.
    NotStarted,
}

/// What wsxd keeps per panel: the last finished run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanelRun {
    pub status: PanelStatus,
    /// Plain text without ANSI codes, at most `MAX_OUTPUT_BYTES`; `truncated` when its start
    /// was cut.
    pub output: String,
    pub truncated: bool,
    pub exit: Option<i32>,
    pub finished_unix_ms: u64,
    /// With `format: json` and a good document: the typed view; `output` stays the raw text.
    #[serde(default)]
    pub doc: Option<PanelDoc>,
    /// What the parse skipped or why `doc` is empty; one line each.
    #[serde(default)]
    pub notice: Option<String>,
}

/// A JSON panel's document: sections of items, checked and cut to the limits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanelDoc {
    pub title: Option<String>,
    pub sections: Vec<Section>,
    /// The worst `warn` or `bad` level of the items; set only by `machine_view`, for the card's title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<Level>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Section {
    pub title: Option<String>,
    pub items: Vec<Item>,
}

/// The theme's state colours, by word.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Ok,
    Warn,
    Bad,
}

/// A number the parse found finite (JSON has no NaN), so `Eq` holds.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Num(pub f64);
impl Eq for Num {}

/// One item. A string field also takes a number; an unknown field is ignored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Item {
    /// A bar of `used` out of `total` (`total` > 0, `used` >= 0).
    Gauge {
        #[serde(deserialize_with = "scalar")]
        label: String,
        used: Num,
        total: Num,
        #[serde(default, deserialize_with = "opt_scalar")]
        unit: Option<String>,
        #[serde(default)]
        level: Option<Level>,
    },
    Stat {
        #[serde(deserialize_with = "scalar")]
        label: String,
        #[serde(deserialize_with = "scalar")]
        value: String,
        #[serde(default, deserialize_with = "opt_scalar")]
        note: Option<String>,
        #[serde(default)]
        level: Option<Level>,
    },
    Table {
        #[serde(deserialize_with = "scalars")]
        columns: Vec<String>,
        #[serde(deserialize_with = "scalar_rows")]
        rows: Vec<Vec<String>>,
    },
    List {
        #[serde(deserialize_with = "scalars")]
        items: Vec<String>,
        #[serde(default)]
        level: Option<Level>,
    },
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Scalar {
    Text(String),
    Number(serde_json::Number),
}

impl Scalar {
    /// The text, cut at `MAX_STRING_CHARS` with a mark.
    fn text(self) -> String {
        let text = match self {
            Self::Text(text) => text,
            Self::Number(number) => number.to_string(),
        };
        if text.chars().count() <= MAX_STRING_CHARS {
            return text;
        }
        let mut cut: String = text.chars().take(MAX_STRING_CHARS - 1).collect();
        cut.push('…');
        cut
    }
}

fn scalar<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    Scalar::deserialize(d).map(Scalar::text)
}

fn opt_scalar<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Option::<Scalar>::deserialize(d).map(|text| text.map(Scalar::text))
}

fn scalars<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    Vec::<Scalar>::deserialize(d).map(|all| all.into_iter().map(Scalar::text).collect())
}

fn scalar_rows<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Vec<String>>, D::Error> {
    Vec::<Vec<Scalar>>::deserialize(d).map(|rows| {
        rows.into_iter()
            .map(|row| row.into_iter().map(Scalar::text).collect())
            .collect()
    })
}

#[derive(Deserialize)]
struct RawDoc {
    #[serde(default, deserialize_with = "opt_scalar")]
    title: Option<String>,
    sections: Vec<Value>,
}

#[derive(Deserialize)]
struct RawSection {
    #[serde(default, deserialize_with = "opt_scalar")]
    title: Option<String>,
    items: Vec<Value>,
}

/// Note once, in the order found, what a limit cut.
fn mark(cut: &mut Vec<&'static str>, what: &'static str) {
    if !cut.contains(&what) {
        cut.push(what);
    }
}

/// `item` when it is drawable: a gauge needs `total` > 0 and `used` >= 0, a table at most
/// `MAX_COLUMNS` columns with rows as long as its columns (rows past `MAX_ROWS` are cut and
/// the list past `MAX_ITEMS`).
fn checked(mut item: Item, cut: &mut Vec<&'static str>) -> Result<Item, String> {
    match &mut item {
        Item::Gauge { used, total, .. } if !(total.0 > 0.0 && used.0 >= 0.0) => {
            return Err("a gauge needs total > 0 and used >= 0".into());
        }
        Item::Table { columns, rows } => {
            if columns.is_empty() || columns.len() > MAX_COLUMNS {
                return Err(format!("a table has 1..={MAX_COLUMNS} columns"));
            }
            if rows.iter().any(|row| row.len() != columns.len()) {
                return Err("a table row must have as many cells as columns".into());
            }
            if rows.len() > MAX_ROWS {
                rows.truncate(MAX_ROWS);
                mark(cut, "table rows");
            }
        }
        Item::List { items, .. } if items.len() > MAX_ITEMS => {
            items.truncate(MAX_ITEMS);
            mark(cut, "list items");
        }
        _ => {}
    }
    Ok(item)
}

/// Read `text` (a `format: json` panel's output) as a `PanelDoc`. An unknown kind, a bad level
/// or value skips that item; anything over a limit is cut; both leave a notice line. Text that
/// is not a document gives no `doc`, only the notice, so the clients show the text.
fn parse_doc(text: &str) -> (Option<PanelDoc>, Option<String>) {
    let raw = match serde_json::from_str::<RawDoc>(text) {
        Ok(raw) => raw,
        Err(error) => return (None, Some(format!("not a panel document: {error}"))),
    };
    let mut notes: Vec<String> = Vec::new();
    let mut cut: Vec<&'static str> = Vec::new();
    if raw.sections.len() > MAX_SECTIONS {
        mark(&mut cut, "sections");
    }
    let mut sections = Vec::new();
    for (number, section) in raw.sections.into_iter().take(MAX_SECTIONS).enumerate() {
        let at = number + 1;
        let section = match serde_json::from_value::<RawSection>(section) {
            Ok(section) => section,
            Err(error) => {
                notes.push(format!("section {at}: {error}"));
                continue;
            }
        };
        if section.items.len() > MAX_ITEMS {
            mark(&mut cut, "items");
        }
        let mut items = Vec::new();
        for (index, item) in section.items.into_iter().take(MAX_ITEMS).enumerate() {
            match serde_json::from_value::<Item>(item)
                .map_err(|error| error.to_string())
                .and_then(|item| checked(item, &mut cut))
            {
                Ok(item) => items.push(item),
                Err(error) => notes.push(format!("section {at}, item {}: {error}", index + 1)),
            }
        }
        sections.push(Section {
            title: section.title,
            items,
        });
    }
    if sections.is_empty() {
        notes.push("no section to show".into());
        return (None, Some(notes.join("\n")));
    }
    if !cut.is_empty() {
        notes.push(format!("cut to the limits: {}", cut.join(", ")));
    }
    if notes.len() > MAX_NOTICES {
        let more = notes.len() - MAX_NOTICES;
        notes.truncate(MAX_NOTICES);
        notes.push(format!("and {more} more"));
    }
    let notice = (!notes.is_empty()).then(|| notes.join("\n"));
    let doc = PanelDoc {
        title: raw.title,
        sections,
        level: None,
    };
    (Some(doc), notice)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    List,
    /// The cached result; `run` starts a run now (open, manual refresh). A run also starts when
    /// the result is older than the panel's `refresh` (not for `refresh: 0`).
    Get {
        name: String,
        run: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanelInfo {
    pub name: String,
    pub refresh: u64,
    /// ^ Absent from a wsxd before revision 45: then `Project`.
    #[serde(default)]
    pub scope: PanelScope,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanelView {
    pub name: String,
    pub refresh: u64,
    /// `Pending` until a run has finished.
    pub run: PanelRun,
    /// Milliseconds since the run finished, by wsxd's clock; 0 while `Pending`.
    pub age_ms: u64,
    pub running: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum Response {
    /// `notice` is the project config's parse problem: its panels are then unavailable.
    Panels {
        panels: Vec<PanelInfo>,
        notice: Option<String>,
    },
    Panel(PanelView),
    Error(String),
}

/// Send `action` for `project` to `client`'s wsxd, which runs the panel. No project: wsxd's
/// built-in machine panels.
pub fn request(
    client: &crate::runtime::Client,
    project: Option<&Path>,
    action: Action,
) -> Result<Response, String> {
    let request = crate::runtime::Request::Panel {
        project: project.map(Path::to_path_buf),
        action,
    };
    match client.call(&request) {
        Ok(crate::runtime::Response::Panel(Response::Error(message))) => Err(message),
        Ok(crate::runtime::Response::Panel(response)) => Ok(response),
        // ^ A wsxd from before panels (revision 43) refuses the request.
        Ok(crate::runtime::Response::Error(error)) => Err(format!(
            "wsxd has no panels ({}); run `wsx daemon restart`",
            error.message
        )),
        Ok(other) => Err(format!("unexpected wsxd answer: {other:?}")),
        Err(error) => Err(format!("{error}")),
    }
}

/// The machine view (docs/panels.md "Machine panels"): wsxd's built-in panel with the sections of
/// every `scope: machine` panel of `projects` after its own, each headed `project · section`. A
/// project panel that failed, timed out or could not be read is one short line in its place.
/// `doc.level` is the worst `warn` or `bad` of all items. Its `refresh` and `age_ms` are the
/// panel's due soonest, so a client that reads again then keeps every section fresh.
pub fn machine_view(
    client: &crate::runtime::Client,
    projects: &[(String, PathBuf)],
    run: bool,
) -> Result<PanelView, String> {
    let name = match request(client, None, Action::List)? {
        Response::Panels { panels, .. } => panels
            .into_iter()
            .find(|info| info.scope == PanelScope::Machine)
            .map(|info| info.name)
            .ok_or("wsxd has no machine panel")?,
        _ => return Err("unexpected wsxd answer".into()),
    };
    let mut view = read_panel(client, None, name, run)?;
    let (mut due, mut busy) = (None, false);
    let mut note = |read: &PanelView| {
        busy |= read.running || read.run.status == PanelStatus::Pending;
        if read.refresh > 0 && read.run.status != PanelStatus::Pending {
            let this = (remaining(read), read.refresh, read.age_ms);
            due = Some(due.map_or(this, |was: (u64, u64, u64)| was.min(this)));
        }
    };
    note(&view);
    let mut sections = Vec::new();
    for (project, path) in projects {
        let Ok(Response::Panels { panels, .. }) = request(client, Some(path), Action::List) else {
            continue;
        };
        for info in panels
            .into_iter()
            .filter(|i| i.scope == PanelScope::Machine)
        {
            let result = read_panel(client, Some(path), info.name.clone(), run);
            if let Ok(read) = &result {
                note(read);
            }
            sections.extend(project_sections(project, &info.name, result));
        }
    }
    // ^ A client reads again at once while any panel runs.
    view.running |= busy;
    if let Some((_, refresh, age_ms)) = due {
        view.refresh = refresh;
        view.age_ms = age_ms;
    }
    if sections.is_empty() {
        return Ok(view);
    }
    let mut doc = view.run.doc.take().unwrap_or(PanelDoc {
        title: None,
        sections: Vec::new(),
        level: None,
    });
    doc.sections.extend(sections);
    doc.level = doc
        .sections
        .iter()
        .flat_map(|section| &section.items)
        .filter_map(|item| match item {
            Item::Gauge { level, .. } | Item::Stat { level, .. } | Item::List { level, .. } => {
                *level
            }
            Item::Table { .. } => None,
        })
        .max()
        .filter(|level| *level >= Level::Warn);
    view.run.output = serde_json::to_string(&doc).unwrap_or_default();
    view.run.doc = Some(doc);
    Ok(view)
}

fn read_panel(
    client: &crate::runtime::Client,
    project: Option<&Path>,
    name: String,
    run: bool,
) -> Result<PanelView, String> {
    match request(client, project, Action::Get { name, run })? {
        Response::Panel(view) => Ok(view),
        _ => Err("unexpected wsxd answer".into()),
    }
}

/// Milliseconds until a panel with a `refresh` is due.
fn remaining(view: &PanelView) -> u64 {
    (view.refresh * 1000).saturating_sub(view.age_ms)
}

/// One project panel's sections in the machine view.
fn project_sections(project: &str, name: &str, result: Result<PanelView, String>) -> Vec<Section> {
    let line = |text: String, level| {
        vec![Section {
            title: Some(project.to_string()),
            items: vec![Item::List {
                items: vec![clip(&text, MAX_STRING_CHARS)],
                level,
            }],
        }]
    };
    let head = |title: Option<String>| match title {
        Some(title) => format!("{project} · {title}"),
        None => project.to_string(),
    };
    let run = match result {
        Ok(view) => view.run,
        Err(message) => {
            let message = message.lines().next().unwrap_or_default();
            return line(format!("{name}: {message}"), Some(Level::Bad));
        }
    };
    match run.status {
        PanelStatus::Pending => line(format!("{name}: no result yet"), None),
        PanelStatus::Failed => line(
            match run.exit {
                Some(code) => format!("{name}: failed (exit {code})"),
                None => format!("{name}: failed (signal)"),
            },
            Some(Level::Bad),
        ),
        PanelStatus::TimedOut => line(format!("{name}: timed out"), Some(Level::Bad)),
        PanelStatus::NotStarted => line(format!("{name}: did not start"), Some(Level::Bad)),
        PanelStatus::Ok => match run.doc {
            Some(doc) => doc
                .sections
                .into_iter()
                .map(|section| Section {
                    title: Some(head(section.title)),
                    items: section.items,
                })
                .collect(),
            None => {
                let mut lines: Vec<String> = run
                    .output
                    .lines()
                    .map(|text| clip(text, MAX_STRING_CHARS))
                    .collect();
                if lines.len() > MAX_ITEMS {
                    let more = lines.len() - MAX_ITEMS + 1;
                    lines.truncate(MAX_ITEMS - 1);
                    lines.push(format!("+{more} more"));
                }
                if lines.is_empty() {
                    lines.push("(no output)".into());
                }
                vec![Section {
                    title: Some(head(Some(name.to_string()))),
                    items: vec![Item::List {
                        items: lines,
                        level: None,
                    }],
                }]
            }
        },
    }
}

/// `text` cut to `max` characters, with `…` where it was cut.
fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(max - 1).collect();
    cut.push('…');
    cut
}

/// Run `panel` in `cwd` (argv, no shell) and wait for it, at most its timeout. The output is
/// stdout, and with a failure stderr after it; its last `MAX_OUTPUT_BYTES` stay.
pub fn run(panel: &PanelConfig, cwd: &Path) -> PanelRun {
    let result = |status, output: String, truncated, exit| PanelRun {
        status,
        output,
        truncated,
        exit,
        finished_unix_ms: crate::task::now_unix_ms(),
        doc: None,
        notice: None,
    };
    let program = Path::new(&panel.command[0]);
    // ^ A relative program such as `scripts/device-status.sh` is the checkout's, whatever
    // the platform does with a relative path after the child's chdir.
    let program = if program.components().count() > 1 && program.is_relative() {
        cwd.join(program)
    } else {
        program.to_path_buf()
    };
    let mut command = Command::new(program);
    command
        .args(&panel.command[1..])
        .current_dir(cwd)
        .env("NO_COLOR", "1")
        .env("TERM", "dumb")
        .stdin(Stdio::null());
    // ^ Its own group, so a timeout stops the command's children too.
    let tail = Bound::Tail(MAX_OUTPUT_BYTES);
    let ran = match run_bounded(
        &mut command,
        &Options {
            timeout: Duration::from_secs(panel.timeout),
            stdout: tail,
            stderr: Stderr::Own(tail),
            reap: false,
        },
    ) {
        Ok(ran) => ran,
        Err(error) => {
            return result(
                PanelStatus::NotStarted,
                format!("could not start {}: {error}", panel.command[0]),
                false,
                None,
            )
        }
    };
    let exit = ran.status.code();
    let status = match (ran.timed_out, exit) {
        (true, _) => PanelStatus::TimedOut,
        (false, Some(0)) => PanelStatus::Ok,
        _ => PanelStatus::Failed,
    };
    let (mut bytes, mut truncated) = (ran.stdout, ran.stdout_cut > 0);
    if status != PanelStatus::Ok {
        truncated |= ran.stderr_cut > 0;
        bytes.extend_from_slice(&ran.stderr);
    }
    if bytes.len() > MAX_OUTPUT_BYTES {
        bytes.drain(..bytes.len() - MAX_OUTPUT_BYTES);
        truncated = true;
    }
    let mut output = plain_text(&String::from_utf8_lossy(&bytes));
    if status == PanelStatus::TimedOut {
        if !output.is_empty() && !output.ends_with('\n') {
            output.push('\n');
        }
        output.push_str(&format!("stopped after {} s", panel.timeout));
    }
    let (doc, notice) = if panel.format == PanelFormat::Json && status == PanelStatus::Ok {
        parse_doc(&output)
    } else {
        (None, None)
    };
    PanelRun {
        doc,
        notice,
        ..result(status, output, truncated, exit)
    }
}

/// `text` without ANSI escape sequences or control characters other than newline and tab;
/// `\r\n` is a newline and a lone `\r` goes.
fn plain_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.next() {
                // CSI: parameters up to a final byte 0x40..=0x7e.
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('\x40'..='\x7e').contains(&c) {
                            break;
                        }
                    }
                }
                // OSC and the like: up to BEL or ST (ESC \).
                Some(']' | 'P' | '_' | '^' | 'X') => {
                    while let Some(c) = chars.next() {
                        if c == '\x07' {
                            break;
                        }
                        if c == '\x1b' {
                            chars.next();
                            break;
                        }
                    }
                }
                // Two-character sequences such as `ESC (` take one more.
                Some('(' | ')' | '*' | '+' | '#' | '%') => {
                    chars.next();
                }
                _ => {}
            },
            '\r' => {}
            '\n' | '\t' => out.push(c),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}
