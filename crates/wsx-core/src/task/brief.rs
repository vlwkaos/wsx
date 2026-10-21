//! The brief a worker writes before it builds: its structured form, the schema the
//! adapter enforces, and `brief.md` as the owner reads it. ref: docs/tasks.md "Brief"

use super::store::{TaskDir, BRIEF_FILE};
use super::MAX_MESSAGE_BYTES;
use serde::{Deserialize, Serialize};
use std::fmt::Write;

/// Largest `brief.json`.
pub const MAX_BRIEF_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Brief {
    /// A small diagram of the flow or structure the task changes: one fenced `mermaid` or
    /// `text` block. Empty in briefs from before it; never a reason to refuse a brief.
    pub diagram: String,
    /// What a top professional would add that the owner did not ask for.
    pub additions: Vec<Addition>,
    /// The primary sources they would start from, by path or link.
    pub raw_material: Vec<Source>,
    pub methods: Vec<String>,
    /// What, other than the owner or an agent, shows the result is right.
    pub independent_check: Vec<String>,
    /// Terms or techniques the owner is likely new to.
    pub concepts: Vec<Concept>,
    /// The task as it should be asked: the build prompt after `start`.
    pub rewritten_prompt: String,
    /// Only the questions whose answer changes the work.
    pub open_questions: Vec<String>,
    /// The answer to the owner's reply in a talk-through turn; empty for the first brief.
    pub answer: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Addition {
    pub what: String,
    pub why: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Source {
    pub source: String,
    pub why: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Concept {
    pub term: String,
    pub explanation: String,
}

/// `brief.json`: the latest version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BriefFile {
    pub version: u32,
    pub brief: Brief,
}

impl Brief {
    /// Refuse a brief the build cannot start from.
    pub fn check(&self) -> Result<(), String> {
        let prompt = self.rewritten_prompt.trim();
        if prompt.is_empty() {
            return Err("the brief has no rewritten prompt".into());
        }
        if prompt.len() > MAX_MESSAGE_BYTES {
            return Err(format!(
                "the rewritten prompt is larger than {MAX_MESSAGE_BYTES} bytes"
            ));
        }
        Ok(())
    }
}

/// The task's latest brief, or none before the first one.
pub fn read(dir: &TaskDir) -> Option<BriefFile> {
    dir.read_file(BRIEF_FILE, MAX_BRIEF_BYTES as u64)
        .ok()
        .flatten()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
}

/// JSON Schema given to the adapter for `Brief`.
pub fn schema() -> serde_json::Value {
    let text = serde_json::json!({ "type": "string" });
    let texts = serde_json::json!({ "type": "array", "items": text });
    let pairs = |a: &str, b: &str| {
        serde_json::json!({
            "type": "array",
            "items": {
                "type": "object",
                "additionalProperties": false,
                "required": [a, b],
                "properties": { a: text, b: text },
            },
        })
    };
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["diagram", "additions", "raw_material", "methods", "independent_check", "concepts",
                     "rewritten_prompt", "open_questions", "answer"],
        "properties": {
            "diagram": text,
            "additions": pairs("what", "why"),
            "raw_material": pairs("source", "why"),
            "methods": texts,
            "independent_check": texts,
            "concepts": pairs("term", "explanation"),
            "rewritten_prompt": text,
            "open_questions": texts,
            "answer": text,
        },
    })
}

/// `brief.md`: the fixed sections, in the order the owner reads them.
pub fn render(id: u32, title: &str, file: &BriefFile) -> String {
    let brief = &file.brief;
    let mut out = format!("# Brief v{} for t{id}: {title}\n", file.version);
    let mut section = |heading: &str, lines: Vec<String>| {
        let _ = write!(out, "\n## {heading}\n\n");
        if lines.is_empty() {
            out.push_str("None.\n");
        }
        for line in lines {
            let _ = writeln!(out, "{line}");
        }
    };
    section("Diagram", text_lines(&brief.diagram));
    section(
        "What a top professional would add",
        brief
            .additions
            .iter()
            .map(|item| format!("- **{}**: {}", item.what.trim(), item.why.trim()))
            .collect(),
    );
    section(
        "Raw material",
        brief
            .raw_material
            .iter()
            .map(|item| format!("- `{}`: {}", item.source.trim(), item.why.trim()))
            .collect(),
    );
    section("Methods and tools", bullets(&brief.methods));
    section("Independent check", bullets(&brief.independent_check));
    section(
        "Concepts to know",
        brief
            .concepts
            .iter()
            .map(|item| format!("- **{}**: {}", item.term.trim(), item.explanation.trim()))
            .collect(),
    );
    section("Rewritten prompt", text_lines(&brief.rewritten_prompt));
    section("Open questions", bullets(&brief.open_questions));
    out
}

/// A free text as one block, or nothing when it is empty.
fn text_lines(text: &str) -> Vec<String> {
    let text = text.trim();
    if text.is_empty() {
        Vec::new()
    } else {
        vec![text.to_string()]
    }
}

fn bullets(items: &[String]) -> Vec<String> {
    items
        .iter()
        .map(|item| format!("- {}", item.trim()))
        .collect()
}
