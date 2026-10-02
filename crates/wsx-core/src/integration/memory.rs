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
const MAX_FILES: usize = 2048;
const MAX_RECORDS: usize = 4096;

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
        if bytes == 0 {
            let mut memory = Memory::unavailable("output_limit");
            memory.truncated = true;
            return memory;
        }
        if !matches!(provider, "claude" | "pi" | "omp" | "codex") {
            return Memory::unavailable("unsupported_provider");
        }
        let Some(reference) = reference else {
            return Memory::unavailable("identity_unavailable");
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
                return Memory::unavailable("inventory_limit");
            }
            match candidates.as_slice() {
                [path] => (*path).clone(),
                [] => return Memory::unavailable("history_not_found"),
                _ => return Memory::unavailable("ambiguous_history"),
            }
        };
        match read_file(provider, reference, &path, messages, bytes) {
            Ok(memory) => memory,
            Err(status) => {
                let mut memory = Memory::unavailable(status);
                memory.path = Some(path);
                memory
            }
        }
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

fn read_file(
    provider: &str,
    reference: &AgentSessionRef,
    path: &Path,
    count: usize,
    budget: usize,
) -> Result<Memory, &'static str> {
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
    let start = metadata.len().saturating_sub(MAX_READ);
    file.seek(SeekFrom::Start(start))
        .map_err(|_| "history_unreadable")?;
    let mut data = Vec::new();
    // Freeze the observed end: a growing log cannot make this read unbounded.
    file.take(metadata.len() - start)
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
    let mut selected = Vec::new();
    if provider == "codex" {
        selected = nodes.iter().collect();
    } else if let Some(last) = nodes.last() {
        let index: HashMap<_, _> = nodes
            .iter()
            .filter_map(|n| n.id.as_deref().map(|id| (id, n)))
            .collect();
        let mut current = Some(last);
        let mut seen = HashSet::new();
        while let Some(node) = current {
            if let Some(id) = node.id.as_deref() {
                if !seen.insert(id) {
                    truncated = true;
                    break;
                }
            }
            selected.push(node);
            current = node.parent.as_deref().and_then(|id| {
                let next = index.get(id).copied();
                if next.is_none() {
                    truncated = true
                }
                next
            });
        }
        selected.reverse();
    }
    let visible: Vec<_> = selected
        .into_iter()
        .filter(|node| !node.text.is_empty())
        .collect();
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
    Ok(Memory {
        source: "provider_native_persisted_history",
        status: if output.is_empty() && checkpoint.is_none() {
            "no_readable_messages"
        } else {
            "available"
        },
        path: Some(path.into()),
        messages: output,
        checkpoint,
        truncated,
    })
}

struct Node {
    id: Option<String>,
    parent: Option<String>,
    role: String,
    text: String,
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
    let role = if summary {
        "summary"
    } else {
        message.get("role").and_then(Value::as_str).unwrap_or(kind)
    };
    let visible = matches!(role, "user" | "assistant" | "summary");
    if provider == "codex"
        && !summary
        && !(kind == "response_item"
            && message.get("type").and_then(Value::as_str) == Some("message"))
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
    Some(Node {
        id: record.get(key).and_then(Value::as_str).map(str::to_owned),
        parent: record
            .get(parent)
            .and_then(Value::as_str)
            .map(str::to_owned),
        role: role.into(),
        text,
    })
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
