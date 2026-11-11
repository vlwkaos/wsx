//! One record, one log line, and the fold of the log. ref: docs/memory.md "Record"

use super::Error;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashSet};
use std::str::FromStr;

pub const MAX_TEXT_BYTES: usize = 4096;
pub const EXCERPT_CHARS: usize = 240;
/// A new text this close to an active record (Jaccard index of the word sets) is a duplicate.
const DUPLICATE_JACCARD: f64 = 0.9;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Rule,
    Fact,
    Episode,
}

impl FromStr for Kind {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, String> {
        match value {
            "rule" => Ok(Self::Rule),
            "fact" => Ok(Self::Fact),
            "episode" => Ok(Self::Episode),
            _ => Err(format!("kind {value:?}: use rule, fact or episode")),
        }
    }
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rule => "rule",
            Self::Fact => "fact",
            Self::Episode => "episode",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Active,
    Superseded,
    Failed,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Superseded => "superseded",
            Self::Failed => "failed",
        }
    }
}

/// The current state of a slug. A log line is the whole record after one op, so the old text of
/// an update stays in the log and the fold is the last line of each slug.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub slug: String,
    pub kind: Kind,
    pub status: Status,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub source: String,
    pub updated: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lesson: Option<String>,
    /// Fields of another writer that this one does not know; kept on every rewrite.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Serialize, Deserialize)]
pub(super) struct Line {
    pub op: String,
    #[serde(flatten)]
    pub record: Record,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Hit {
    pub slug: String,
    pub scope: String,
    pub path: String,
    pub kind: Kind,
    pub status: Status,
    pub score: f64,
    pub excerpt: String,
}

pub(super) struct Folded {
    pub records: BTreeMap<String, Record>,
    /// Lines that did not parse: a write cut short by a crash.
    pub skipped: usize,
}

pub(super) fn fold(bytes: &[u8]) -> Folded {
    let mut folded = Folded {
        records: BTreeMap::new(),
        skipped: 0,
    };
    for line in bytes.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
        match serde_json::from_slice::<Line>(line) {
            Ok(line) => {
                folded.records.insert(line.record.slug.clone(), line.record);
            }
            Err(_) => folded.skipped += 1,
        }
    }
    folded
}

/// Lowercased words: runs of alphanumeric characters.
fn words(text: &str) -> HashSet<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

/// An active record of `kind` that `text` repeats: the same words in the same order after
/// lowercasing, or a word set with a Jaccard index of 0.9 or more.
pub(super) fn duplicate<'a>(
    records: &'a BTreeMap<String, Record>,
    kind: Kind,
    text: &str,
) -> Option<&'a Record> {
    let normal = |t: &str| {
        t.to_lowercase()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    };
    let (wanted, new_words) = (normal(text), words(text));
    records.values().find(|r| {
        if r.status != Status::Active || r.kind != kind {
            return false;
        }
        if normal(&r.text) == wanted {
            return true;
        }
        let old = words(&r.text);
        let union = old.union(&new_words).count();
        union > 0 && old.intersection(&new_words).count() as f64 / union as f64 >= DUPLICATE_JACCARD
    })
}

/// Share of the query's words that the record's slug, title or text holds; 0 when none.
pub(super) fn score(record: &Record, query: &HashSet<String>) -> f64 {
    let held = words(&format!(
        "{} {} {}",
        record.slug,
        record.title.as_deref().unwrap_or(""),
        record.text
    ));
    query.iter().filter(|w| held.contains(*w)).count() as f64 / query.len() as f64
}

pub(super) fn query_words(query: &str) -> Result<HashSet<String>, Error> {
    let set = words(query);
    if set.is_empty() {
        return Err(Error::Usage("the query has no words".into()));
    }
    Ok(set)
}
