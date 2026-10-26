//! Bounded, read-only projections of provider-native session history.
//! ^ Native records are untrusted evidence, not model context or exchange receipts.
//! See docs/agent-orchestration.md. No provider process or model is started here.
use crate::runtime::{AgentSessionRef, AgentSessionRefKind};
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    fs::{self, OpenOptions},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

const MAX_READ: u64 = 1024 * 1024;
const MAX_GROWN_READ: u64 = 16 * 1024 * 1024;
const MAX_FILES: usize = 2048;
const MAX_RECORDS: usize = 4096;
const MAX_TOOL_TITLE: usize = 120;
const MAX_TOOL_NAME: usize = 64;
const MAX_ITEM_ID: usize = 96;
const MAX_TOOL_INPUT: usize = 64 * 1024;

#[derive(Debug, Serialize)]
pub struct Message {
    pub role: String,
    pub text: String,
}
#[derive(Debug, Serialize)]
pub struct Memory {
    pub source: &'static str,
    pub status: &'static str,
    pub path: Option<PathBuf>,
    pub messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<Message>,
    pub truncated: bool,
}
impl Memory {
    fn unavailable(status: &'static str) -> Self {
        Self {
            source: "provider_native_persisted_history",
            status,
            path: None,
            messages: Vec::new(),
            checkpoint: None,
            truncated: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    User,
    Assistant,
    Tool,
    Thinking,
    Summary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Running,
    Ok,
    Error,
}

#[derive(Debug, Serialize)]
pub struct ToolCall {
    pub name: String,
    /// One line built from the call's input, never the input itself.
    pub title: String,
    pub status: ToolStatus,
}

/// One entry of the typed timeline. A thinking item says only that thinking happened.
#[derive(Debug, Serialize)]
pub struct Item {
    pub id: String,
    pub kind: ItemKind,
    pub at_unix_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<ToolCall>,
}

/// The same conversation as `Memory`, with tool calls and thinking kept as items.
#[derive(Debug, Serialize)]
pub struct Timeline {
    pub source: &'static str,
    pub status: &'static str,
    pub path: Option<PathBuf>,
    /// Oldest first. The newest compaction summary is an item where the compaction
    /// happened, or the first item when it is older than the items kept.
    pub items: Vec<Item>,
    pub truncated: bool,
    /// The model of the newest record that names one: a reply, or a switch the person made.
    pub model: Option<String>,
}

/// The agent's last message after one submitted input.
#[derive(Debug, Serialize)]
pub struct Reply {
    pub source: &'static str,
    /// `available`, or the reason why no message can be named.
    pub status: &'static str,
    pub path: Option<PathBuf>,
    pub text: Option<String>,
    pub truncated: bool,
}

/// Reuse one bounded store inventory per provider during multi-agent discovery.
#[derive(Default)]
pub struct Reader {
    inventories: HashMap<String, (Vec<PathBuf>, bool)>,
}
impl Reader {
    pub fn read(
        &mut self,
        provider: &str,
        reference: Option<&AgentSessionRef>,
        messages: usize,
        bytes: usize,
    ) -> Memory {
        match self.load(provider, reference, bytes) {
            Ok(loaded) => messages_of(loaded, messages, bytes),
            Err((status, path)) => {
                let mut memory = Memory::unavailable(status);
                memory.truncated = status == "output_limit";
                memory.path = path;
                memory
            }
        }
    }

    /// The typed projection of the same parsed records: messages, tool calls with
    /// their outcome, and thinking markers. `items` and `bytes` bound the newest part.
    pub fn read_timeline(
        &mut self,
        provider: &str,
        reference: Option<&AgentSessionRef>,
        items: usize,
        bytes: usize,
    ) -> Timeline {
        match self.load(provider, reference, bytes) {
            Ok(loaded) => timeline_of(loaded, items, bytes),
            Err((status, path)) => Timeline {
                source: "provider_native_persisted_history",
                status,
                path,
                items: Vec::new(),
                truncated: status == "output_limit",
                model: None,
            },
        }
    }

    /// The last assistant message after the newest submitted input that contains `marker`.
    /// ^ docs/agent-orchestration.md "Result of a round": a message from before that input
    /// is never returned. An input that is not in the history, or has no message after it,
    /// is a status, not an older message.
    pub fn read_reply(
        &mut self,
        provider: &str,
        reference: Option<&AgentSessionRef>,
        marker: &str,
        bytes: usize,
    ) -> Reply {
        let mut reply = Reply {
            source: "provider_native_persisted_history",
            status: "available",
            path: None,
            text: None,
            truncated: false,
        };
        let loaded = match self.load(provider, reference, bytes) {
            Ok(loaded) => loaded,
            Err((status, path)) => {
                reply.status = status;
                reply.path = path;
                return reply;
            }
        };
        let nodes = &loaded.nodes;
        reply.path = Some(loaded.path.clone());
        let Some(start) = nodes
            .iter()
            .rposition(|node| node.submitted() && node.text.contains(marker))
        else {
            reply.status = "delivery_not_in_history";
            return reply;
        };
        // The round ends where the next input was submitted.
        let end = nodes[start + 1..]
            .iter()
            .position(Node::submitted)
            .map_or(nodes.len(), |offset| start + 1 + offset);
        match nodes[start + 1..end]
            .iter()
            .rev()
            .find(|node| node.role == "assistant" && !node.text.is_empty())
        {
            Some(node) => {
                let text = clip(&node.text, bytes);
                reply.truncated = text.len() < node.text.len();
                reply.text = Some(text);
            }
            None => reply.status = "no_message_after_delivery",
        }
        reply
    }

    fn load(
        &mut self,
        provider: &str,
        reference: Option<&AgentSessionRef>,
        bytes: usize,
    ) -> Result<Loaded, (&'static str, Option<PathBuf>)> {
        if bytes == 0 {
            return Err(("output_limit", None));
        }
        if !matches!(provider, "claude" | "pi" | "omp" | "codex") {
            return Err(("unsupported_provider", None));
        }
        let Some(reference) = reference else {
            return Err(("identity_unavailable", None));
        };
        let explicit = reference.transcript_path.as_deref().or_else(|| {
            (reference.kind == AgentSessionRefKind::Path).then(|| Path::new(&reference.value))
        });
        let path = if let Some(path) = explicit {
            path.to_owned()
        } else {
            // Never interpolate a provider ID into a path or search unrelated home files.
            let inventory = self.inventories.entry(provider.into()).or_insert_with(|| {
                let Some(home) = dirs::home_dir() else {
                    return (Vec::new(), false);
                };
                let root = match provider {
                    "claude" => std::env::var_os("CLAUDE_CONFIG_DIR")
                        .map(PathBuf::from)
                        .unwrap_or_else(|| home.join(".claude"))
                        .join("projects"),
                    "codex" => std::env::var_os("CODEX_HOME")
                        .map(PathBuf::from)
                        .unwrap_or_else(|| home.join(".codex"))
                        .join("sessions"),
                    "pi" => home.join(".pi/agent/sessions"),
                    _ => home.join(".omp/agent/sessions"),
                };
                let mut files = Vec::new();
                let mut visited = 0;
                let mut truncated = false;
                inventory_files(&root, 5, &mut visited, &mut files, &mut truncated);
                (files, truncated)
            });
            let suffix = format!("_{}", reference.value);
            let codex_suffix = format!("-{}", reference.value);
            let candidates: Vec<_> = inventory
                .0
                .iter()
                .filter(|path| {
                    let name = path.file_stem().and_then(|v| v.to_str()).unwrap_or("");
                    name == reference.value
                        || name.ends_with(&suffix)
                        || name.ends_with(&codex_suffix)
                })
                .collect();
            if inventory.1 {
                return Err(("inventory_limit", None));
            }
            match candidates.as_slice() {
                [path] => (*path).clone(),
                [] => return Err(("history_not_found", None)),
                _ => return Err(("ambiguous_history", None)),
            }
        };
        read_file(provider, reference, &path).map_err(|status| (status, Some(path)))
    }
}

fn inventory_files(
    root: &Path,
    depth: usize,
    visited: &mut usize,
    files: &mut Vec<PathBuf>,
    truncated: &mut bool,
) {
    if depth == 0 || fs::symlink_metadata(root).is_ok_and(|m| m.file_type().is_symlink()) {
        return;
    }
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        *visited += 1;
        if *visited > MAX_FILES {
            *truncated = true;
            return;
        }
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            inventory_files(&entry.path(), depth - 1, visited, files, truncated)
        } else if kind.is_file() && entry.path().extension().is_some_and(|v| v == "jsonl") {
            files.push(entry.path())
        }
        if *truncated {
            return;
        }
    }
}

/// The parsed, bounded conversation both projections start from: the records
/// on the path from the newest one back to the root, oldest first.
struct Loaded {
    path: PathBuf,
    nodes: Vec<Node>,
    truncated: bool,
    malformed: bool,
}

fn read_file(
    provider: &str,
    reference: &AgentSessionRef,
    path: &Path,
) -> Result<Loaded, &'static str> {
    if path.extension().is_none_or(|v| v != "jsonl") {
        return Err("unsupported_format");
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // No FIFO/device reads or final-component symlink traversal.
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut file = options.open(path).map_err(|_| "history_unreadable")?;
    let metadata = file.metadata().map_err(|_| "history_unreadable")?;
    if !metadata.is_file() {
        return Err("unsafe_history_file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != unsafe { libc::geteuid() } {
            return Err("history_owner_mismatch");
        }
    }
    // ^ A long turn (tool results, Claude's attachments) can push the person's last message
    // out of the newest MiB: read further back, up to MAX_GROWN_READ, until it is in.
    // docs/web-client-chat.md "Timeline"
    let mut window = MAX_READ;
    loop {
        let start = metadata.len().saturating_sub(window);
        let loaded = read_tail(provider, reference, path, &mut file, start, metadata.len());
        let short = start > 0
            && window < MAX_GROWN_READ
            && loaded.as_ref().map_or_else(
                |status| *status == "record_limit",
                |loaded| loaded.truncated && !loaded.nodes.iter().any(Node::submitted),
            );
        if !short {
            return loaded;
        }
        window *= 4;
    }
}

/// The records from `start` to `end`, the observed end of the file.
fn read_tail(
    provider: &str,
    reference: &AgentSessionRef,
    path: &Path,
    file: &mut fs::File,
    start: u64,
    end: u64,
) -> Result<Loaded, &'static str> {
    file.seek(SeekFrom::Start(start))
        .map_err(|_| "history_unreadable")?;
    let mut data = Vec::new();
    // Freeze the observed end: a growing log cannot make this read unbounded.
    file.take(end - start)
        .read_to_end(&mut data)
        .map_err(|_| "history_unreadable")?;
    let mut truncated = start > 0;
    if start > 0 {
        let Some(end) = data.iter().position(|b| *b == b'\n') else {
            return Err("record_limit");
        };
        data.drain(..=end);
    }
    let mut nodes = Vec::new();
    let mut malformed = false;
    for line in data.split(|b| *b == b'\n').filter(|line| !line.is_empty()) {
        let Ok(record) = serde_json::from_slice::<Value>(line) else {
            malformed = true;
            continue;
        };
        if record.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        if reference.kind == AgentSessionRefKind::Id
            && record
                .get("sessionId")
                .and_then(Value::as_str)
                .is_some_and(|id| id != reference.value)
        {
            continue;
        }
        if let Some(node) = node(provider, &record) {
            nodes.push(node)
        }
        if nodes.len() > MAX_RECORDS {
            nodes.remove(0);
            truncated = true
        }
    }
    if provider != "codex" {
        // Keep only the branch that ends at the newest record.
        let mut keep = vec![false; nodes.len()];
        // Records a Claude compaction kept, and the summary they follow.
        let mut moved = Vec::new();
        let mut anchor = None;
        if !nodes.is_empty() {
            let index: HashMap<_, _> = nodes
                .iter()
                .enumerate()
                .filter_map(|(position, node)| node.id.as_deref().map(|id| (id, position)))
                .collect();
            let mut current = Some(nodes.len() - 1);
            let mut seen = HashSet::new();
            let mut segment_end: Option<&str> = None;
            while let Some(position) = current {
                let node = &nodes[position];
                if let Some(id) = node.id.as_deref() {
                    if !seen.insert(id) {
                        truncated = true;
                        break;
                    }
                }
                keep[position] = true;
                if segment_end.is_some() {
                    moved.push(position);
                    if node.id.as_deref() == segment_end {
                        break;
                    }
                }
                // ^ Claude 2.1.300: a compaction boundary starts a new chain, and its
                // `preservedSegment` names the newest old records the model still has,
                // after the summary. The walk goes on from the last of them to the first.
                let next = match (&node.preserved, segment_end) {
                    (Some(preserved), None) => {
                        segment_end = Some(&preserved.head);
                        anchor = index.get(preserved.anchor.as_str()).copied();
                        Some(preserved.tail.as_str())
                    }
                    _ => node.parent.as_deref(),
                };
                // ^ Claude 2.1.30x saves a `deferred_tools_record` attachment whose parent was
                // never saved: the chain goes on at the record saved before it. Any other missing
                // parent ends the chain, so a branch the person left never joins it.
                current = next.and_then(|id| match index.get(id) {
                    Some(next) => Some(*next),
                    None if node.role == "attachment" && position > 0 => Some(position - 1),
                    None => {
                        truncated = true;
                        None
                    }
                });
            }
        }
        // Branch records in file order, with the kept records right after their summary.
        moved.reverse();
        let mut slots: Vec<Option<Node>> = nodes.into_iter().map(Some).collect();
        let mut order = Vec::new();
        for (position, kept) in keep.iter().enumerate() {
            if *kept && !moved.contains(&position) {
                order.push(position);
                if Some(position) == anchor {
                    order.extend(&moved);
                }
            }
        }
        if anchor.is_none_or(|anchor| !keep[anchor]) {
            truncated |= !moved.is_empty();
        }
        nodes = order
            .into_iter()
            .filter_map(|position| slots[position].take())
            .collect();
    }
    Ok(Loaded {
        path: path.into(),
        nodes,
        truncated,
        malformed,
    })
}

fn messages_of(loaded: Loaded, count: usize, budget: usize) -> Memory {
    let Loaded {
        path,
        nodes,
        mut truncated,
        malformed,
    } = loaded;
    let visible: Vec<_> = nodes.iter().filter(|node| !node.text.is_empty()).collect();
    // ^ Keep the latest persisted compaction even when it falls outside the
    // recent-message tail. Split one fixed budget, never replay full history.
    let summary = visible.iter().rev().find(|node| node.role == "summary");
    let checkpoint = summary.and_then(|node| {
        let text = clip(&node.text, budget / 2);
        truncated |= text.len() < node.text.len();
        (!text.is_empty()).then(|| Message {
            role: "summary".into(),
            text,
        })
    });
    let mut remaining = budget - checkpoint.as_ref().map_or(0, |message| message.text.len());
    let recent: Vec<_> = visible
        .iter()
        .filter(|node| node.role != "summary")
        .collect();
    let mut output = Vec::new();
    for node in recent.iter().rev().take(count) {
        if remaining == 0 {
            truncated = true;
            break;
        }
        let text = clip(&node.text, remaining);
        truncated |= text.len() < node.text.len();
        remaining -= text.len();
        output.push(Message {
            role: node.role.clone(),
            text,
        });
    }
    truncated |= recent.len() > count || malformed;
    output.reverse();
    Memory {
        source: "provider_native_persisted_history",
        status: if output.is_empty() && checkpoint.is_none() {
            "no_readable_messages"
        } else {
            "available"
        },
        path: Some(path),
        messages: output,
        checkpoint,
        truncated,
    }
}

fn timeline_of(loaded: Loaded, count: usize, budget: usize) -> Timeline {
    let Loaded {
        path,
        nodes,
        mut truncated,
        malformed,
    } = loaded;
    let model = nodes.iter().rev().find_map(|node| node.model.clone());
    // Outcome of each call, and where the last turn starts: at the newest message
    // a person wrote. A call without a result is running only inside that turn.
    let mut results: HashMap<&str, bool> = HashMap::new();
    let mut last_turn = 0;
    for (position, node) in nodes.iter().enumerate() {
        for step in &node.steps {
            if let Step::Result { call, error } = step {
                results.insert(call, *error);
            }
        }
        if node.submitted() {
            last_turn = position;
        }
    }
    let item_id = |position: usize, suffix: String| {
        let id = nodes[position].id.as_deref().map_or_else(
            || format!("r{position}{suffix}"),
            |id| format!("{}{suffix}", clean(id, MAX_ITEM_ID)),
        );
        clean(&id, MAX_ITEM_ID + 8)
    };
    // The newest summary keeps half the budget, so a long conversation never hides it.
    let summary = nodes
        .iter()
        .rposition(|node| node.role == "summary" && !node.text.is_empty());
    let mut checkpoint = summary.and_then(|position| {
        let node = &nodes[position];
        let text = clip(&node.text, budget / 2);
        truncated |= text.len() < node.text.len();
        (!text.is_empty()).then(|| Item {
            id: item_id(position, String::new()),
            kind: ItemKind::Summary,
            at_unix_ms: node.at_unix_ms,
            text: Some(text),
            tool: None,
        })
    });
    let mut remaining = budget
        - checkpoint
            .as_ref()
            .and_then(|item| item.text.as_ref())
            .map_or(0, String::len);
    let mut items = Vec::new();
    'nodes: for (position, node) in nodes.iter().enumerate().rev() {
        if node.role == "summary" {
            if Some(position) == summary {
                items.extend(checkpoint.take());
            }
            continue;
        }
        let name = |suffix: String| item_id(position, suffix);
        // Newest first here, so a record's steps go in before its text.
        for (index, step) in node.steps.iter().enumerate().rev() {
            let (kind, id, tool) = match step {
                Step::Result { .. } => continue,
                Step::Thinking => (ItemKind::Thinking, name(format!(":t{index}")), None),
                Step::Call { id, name, title } => {
                    let status = match results.get(id.as_str()) {
                        Some(true) => ToolStatus::Error,
                        Some(false) => ToolStatus::Ok,
                        None if position >= last_turn => ToolStatus::Running,
                        None => ToolStatus::Ok,
                    };
                    let tool = ToolCall {
                        name: name.clone(),
                        title: title.clone(),
                        status,
                    };
                    (ItemKind::Tool, clean(id, MAX_ITEM_ID), Some(tool))
                }
            };
            let cost = tool.as_ref().map_or(8, |tool| tool.title.len() + 8);
            if items.len() == count || remaining < cost {
                truncated = true;
                break 'nodes;
            }
            remaining -= cost;
            items.push(Item {
                id,
                kind,
                at_unix_ms: node.at_unix_ms,
                text: None,
                tool,
            });
        }
        let kind = match node.role.as_str() {
            "user" => ItemKind::User,
            "assistant" => ItemKind::Assistant,
            _ => continue,
        };
        if node.text.is_empty() || node.meta {
            continue;
        }
        if items.len() == count || remaining == 0 {
            truncated = true;
            break;
        }
        let text = clip(&node.text, remaining);
        truncated |= text.len() < node.text.len();
        remaining -= text.len();
        items.push(Item {
            id: name(String::new()),
            kind,
            at_unix_ms: node.at_unix_ms,
            text: Some(text),
            tool: None,
        });
    }
    // A summary older than the kept items still comes first.
    items.extend(checkpoint);
    items.reverse();
    Timeline {
        source: "provider_native_persisted_history",
        status: if items.is_empty() {
            "no_readable_messages"
        } else {
            "available"
        },
        path: Some(path),
        items,
        truncated: truncated || malformed,
        model,
    }
}

/// What a record did besides saying something.
enum Step {
    Call {
        id: String,
        name: String,
        title: String,
    },
    Result {
        call: String,
        error: bool,
    },
    Thinking,
}

struct Node {
    id: Option<String>,
    parent: Option<String>,
    role: String,
    text: String,
    at_unix_ms: Option<u64>,
    /// A record the provider wrote for itself in the person's name.
    meta: bool,
    steps: Vec<Step>,
    model: Option<String>,
    /// A Claude compaction boundary that kept the newest records: the ids of the first
    /// and last kept record, and of the summary they follow.
    preserved: Option<Preserved>,
}

impl Node {
    /// An input the person submitted.
    fn submitted(&self) -> bool {
        self.role == "user" && !self.meta && !self.text.is_empty()
    }
}

struct Preserved {
    head: String,
    tail: String,
    anchor: String,
}
/// Claude's fixed words around a compaction summary, as its transcripts (2.x) write them.
const CLAUDE_SUMMARY_START: &str = "This session is being continued from a previous conversation that ran out of context. The summary below covers the earlier portion of the conversation.\n\nSummary:\n";
const CLAUDE_SUMMARY_END: &str =
    "\n\nIf you need specific details from before compaction (like exact code snippets";

/// The summary itself: the instructions to the model around it, which also name a local
/// transcript path, are not part of what the conversation was about.
fn claude_summary(text: &str) -> String {
    let text = text.strip_prefix(CLAUDE_SUMMARY_START).unwrap_or(text);
    let text = text
        .rfind(CLAUDE_SUMMARY_END)
        .map_or(text, |end| &text[..end]);
    text.trim().to_owned()
}

fn node(provider: &str, record: &Value) -> Option<Node> {
    let kind = record.get("type")?.as_str()?;
    let message = if provider == "codex" {
        record.get("payload")?
    } else {
        record.get("message").unwrap_or(record)
    };
    let summary = match provider {
        "claude" => record.get("isCompactSummary").and_then(Value::as_bool) == Some(true),
        "codex" => kind == "compacted",
        _ => matches!(kind, "compaction" | "branch_summary"),
    };
    // ^ A message the person typed while Claude worked is saved, once a turn takes it, as an
    // `attachment` of type `queued_command` with the words in `prompt` (Claude Code 2.1.309).
    let queued = (provider == "claude" && kind == "attachment")
        .then(|| record.get("attachment"))
        .flatten()
        .filter(|attachment| {
            attachment.get("type").and_then(Value::as_str) == Some("queued_command")
                && attachment.pointer("/origin/kind").and_then(Value::as_str) == Some("human")
        })
        .and_then(|attachment| attachment.get("prompt")?.as_str());
    let role = if summary {
        "summary"
    } else if queued.is_some() {
        "user"
    } else {
        message.get("role").and_then(Value::as_str).unwrap_or(kind)
    };
    let visible = matches!(role, "user" | "assistant" | "summary");
    let steps = if summary {
        Vec::new()
    } else {
        steps(provider, role, message)
    };
    if provider == "codex"
        && !summary
        && kind != "turn_context"
        && !(kind == "response_item"
            && (message.get("type").and_then(Value::as_str) == Some("message")
                || !steps.is_empty()))
    {
        return None;
    }
    let text = if summary && provider != "claude" {
        message
            .get(if provider == "codex" {
                "message"
            } else {
                "summary"
            })
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned()
    } else if summary {
        claude_summary(&content_text(message.get("content")))
    } else if let Some(queued) = queued {
        queued.to_owned()
    } else if visible {
        content_text(message.get("content"))
    } else {
        String::new()
    };
    let key = if provider == "claude" { "uuid" } else { "id" };
    let parent = if provider == "claude" {
        "parentUuid"
    } else {
        "parentId"
    };
    if provider != "codex" && record.get(key).and_then(Value::as_str).is_none() {
        return None;
    }
    let text_is_notice = provider == "claude"
        && (text.starts_with("[Request interrupted by user")
            || text.starts_with("<local-command-stdout>")
            || text.starts_with("<local-command-stderr>")
            || text.starts_with("<bash-stdout>")
            || text.starts_with("<bash-stderr>"));
    // ^ A slash command or a `!` shell line is saved in tags: show it as the person typed it.
    let text = command_line(provider, role, &text).unwrap_or(text);
    let preserved = (provider == "claude")
        .then(|| record.get("compactMetadata")?.get("preservedSegment"))
        .flatten()
        .and_then(|segment| {
            let id = |key| Some(segment.get(key)?.as_str()?.to_owned());
            Some(Preserved {
                head: id("headUuid")?,
                tail: id("tailUuid")?,
                anchor: id("anchorUuid")?,
            })
        });
    // ^ Where each provider names the model (real sessions, 2026-10-20): Claude and Pi on
    // each reply, Pi's `model_change` when the person switches, Codex on each turn's context.
    // Claude's own notices say `<synthetic>`.
    let model = match kind {
        "turn_context" if provider == "codex" => message.get("model"),
        "model_change" => record.get("modelId"),
        _ if role == "assistant" => message.get("model"),
        _ => None,
    }
    .and_then(Value::as_str)
    .filter(|model| !model.trim().is_empty() && !model.starts_with('<'))
    .map(|model| clean(model, 80));
    // A codex record has no id of its own; its payload does.
    let id = record.get(key).or_else(|| message.get("id"));
    Some(Node {
        id: id.and_then(Value::as_str).map(str::to_owned),
        parent: record
            .get(parent)
            .and_then(Value::as_str)
            .map(str::to_owned),
        role: role.into(),
        text,
        at_unix_ms: message
            .get("timestamp")
            .and_then(Value::as_u64)
            .or_else(|| unix_ms(record.get("timestamp")?.as_str()?)),
        // Claude also writes its own interruption notice, a command's output and (2.1.300)
        // a background-task notice with `promptSource: "system"` in the person's name. A
        // prompt the person gave is `typed`, `suggestion_accepted`, `queued` or `sdk`.
        meta: record.get("isMeta").and_then(Value::as_bool) == Some(true)
            || (role == "user" && text_is_notice)
            || (provider == "claude"
                && record.get("promptSource").and_then(Value::as_str) == Some("system")),
        steps,
        model,
        preserved,
    })
}

/// `/name args` from Claude's saved form of a slash command:
/// `<command-name>/name</command-name> <command-message>…</command-message> <command-args>args</command-args>`,
/// and `! command` from a shell line, `<bash-input> command</bash-input>` (Claude Code 2.1.304).
fn command_line(provider: &str, role: &str, text: &str) -> Option<String> {
    if provider != "claude" || role != "user" {
        return None;
    }
    // ^ The text after `!` is kept as typed, so the chat finds its pending message.
    if let Some(shell) = text
        .strip_prefix("<bash-input>")
        .and_then(|rest| rest.strip_suffix("</bash-input>"))
    {
        return Some(format!("!{shell}"));
    }
    let inner = |tag: &str| {
        let start = text.find(&format!("<{tag}>"))? + tag.len() + 2;
        let end = start + text[start..].find(&format!("</{tag}>"))?;
        Some(text[start..end].trim())
    };
    let name = text
        .starts_with("<command-name>")
        .then(|| inner("command-name"))??;
    Some(
        match inner("command-args").filter(|args| !args.is_empty()) {
            Some(args) => format!("{name} {args}"),
            None => name.to_owned(),
        },
    )
}

// ^ Record shapes read from real sessions on 2026-10-12 (docs/web-client-chat.md "Timeline"):
// claude `tool_use`/`tool_result`/`thinking` parts, pi and omp `toolCall`/`thinking`
// parts and `toolResult` messages, codex `function_call`, `custom_tool_call`, their
// `_output` items and `reasoning`.
fn steps(provider: &str, role: &str, message: &Value) -> Vec<Step> {
    let text = |value: &Value, key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
    let call = |id: Option<String>, name: Option<String>, input: Option<&Value>| {
        let name = clean(name.as_deref().unwrap_or("tool"), MAX_TOOL_NAME);
        Some(Step::Call {
            id: id?,
            title: title(&name, input),
            name,
        })
    };
    if provider == "codex" {
        let step = match message.get("type").and_then(Value::as_str) {
            Some("function_call") => {
                // The arguments are a JSON document in a string.
                let input = message
                    .get("arguments")
                    .and_then(Value::as_str)
                    .filter(|arguments| arguments.len() <= MAX_TOOL_INPUT)
                    .and_then(|arguments| serde_json::from_str::<Value>(arguments).ok());
                call(
                    text(message, "call_id"),
                    text(message, "name"),
                    input.as_ref(),
                )
            }
            Some("custom_tool_call") => call(
                text(message, "call_id"),
                text(message, "name"),
                message.get("input"),
            ),
            Some("function_call_output" | "custom_tool_call_output") => {
                text(message, "call_id").map(|call| Step::Result { call, error: false })
            }
            Some("reasoning") => Some(Step::Thinking),
            _ => None,
        };
        return step.into_iter().collect();
    }
    if role == "toolResult" {
        let error = message.get("isError").and_then(Value::as_bool) == Some(true);
        return text(message, "toolCallId")
            .map(|call| Step::Result { call, error })
            .into_iter()
            .collect();
    }
    let Some(Value::Array(parts)) = message.get("content") else {
        return Vec::new();
    };
    parts
        .iter()
        .filter_map(|part| match part.get("type").and_then(Value::as_str)? {
            "thinking" => Some(Step::Thinking),
            "tool_use" => call(text(part, "id"), text(part, "name"), part.get("input")),
            "toolCall" => call(text(part, "id"), text(part, "name"), part.get("arguments")),
            "tool_result" => Some(Step::Result {
                call: text(part, "tool_use_id")?,
                error: part.get("is_error").and_then(Value::as_bool) == Some(true),
            }),
            _ => None,
        })
        .collect()
}

/// One line that says what a call does: its description, else its command, else
/// the file it names, else the first question it asks, else the tool name.
fn title(name: &str, input: Option<&Value>) -> String {
    let line = match input {
        Some(Value::String(text)) => Some(text.clone()),
        Some(Value::Object(fields)) => ["description", "command", "file_path", "path"]
            .iter()
            .find_map(|key| match fields.get(*key)? {
                Value::String(text) => Some(text.clone()),
                Value::Array(words) => Some(
                    words
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(" "),
                ),
                _ => None,
            })
            // A tool that asks the person (Claude's AskUserQuestion): the question itself.
            .or_else(|| {
                let question = fields.get("questions")?.get(0)?.get("question")?;
                question.as_str().map(str::to_owned)
            }),
        _ => None,
    };
    let line = line.unwrap_or_default();
    let line = clean(line.trim().lines().next().unwrap_or(""), MAX_TOOL_TITLE);
    if line.is_empty() {
        name.to_owned()
    } else {
        line
    }
}

/// Untrusted text for display: no control characters, at most `chars` characters.
fn clean(text: &str, chars: usize) -> String {
    text.chars()
        .filter(|character| !character.is_control())
        .take(chars)
        .collect()
}

/// Milliseconds since the epoch of an RFC 3339 UTC time such as `2026-10-12T05:01:02.123Z`.
fn unix_ms(text: &str) -> Option<u64> {
    let number = |range: std::ops::Range<usize>| text.get(range)?.parse::<u64>().ok();
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let (hour, minute, second) = (number(11..13)?, number(14..16)?, number(17..19)?);
    if !(1970..=9999).contains(&year) || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let millis = match text.get(19..20) {
        Some(".") => {
            let digits: String = text[20..]
                .chars()
                .take_while(char::is_ascii_digit)
                .chain("000".chars())
                .take(3)
                .collect();
            digits.parse::<u64>().ok()?
        }
        _ => 0,
    };
    // Days from civil date (proleptic Gregorian), for March-based years.
    let year = if month <= 2 { year - 1 } else { year };
    let era_year = year % 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = era_year * 365 + era_year / 4 - era_year / 100 + day_of_year;
    let days = (year / 400) * 146_097 + day_of_era - 719_468;
    Some((((days * 24 + hour) * 60 + minute) * 60 + second) * 1000 + millis)
}
fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter(|part| {
                matches!(
                    part.get("type").and_then(Value::as_str),
                    Some("text" | "input_text" | "output_text")
                )
            })
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}
fn clip(text: &str, bytes: usize) -> String {
    let mut end = bytes.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1
    }
    text[..end].to_owned()
}
