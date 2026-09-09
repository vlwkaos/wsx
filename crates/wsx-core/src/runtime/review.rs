//! Structured executable review protocol. See docs/worktree-review.md.
//! These types do not grant plugin execution or filesystem authority.

use super::WorktreeId;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const REVIEW_API_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewComparison {
    WorkingAgainstHead,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSpec {
    pub api_version: u32,
    pub priority: i32,
    pub comparisons: Vec<ReviewComparison>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewLimits {
    pub files: usize,
    pub hunks: usize,
    pub lines: usize,
}

impl Default for ReviewLimits {
    fn default() -> Self {
        Self {
            files: 1_000,
            hunks: 256,
            lines: 10_000,
        }
    }
}

impl ReviewLimits {
    pub fn is_valid(&self) -> bool {
        (1..=1_000).contains(&self.files)
            && (1..=256).contains(&self.hunks)
            && (1..=10_000).contains(&self.lines)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewRequest {
    pub api_version: u32,
    pub request_id: String,
    pub worktree_id: WorktreeId,
    pub worktree_path: PathBuf,
    pub comparison: ReviewComparison,
    pub limits: ReviewLimits,
    #[serde(flatten)]
    pub operation: ReviewOperation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum ReviewOperation {
    ListFiles,
    FileDiff { snapshot: String, file_id: String },
}

impl ReviewRequest {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.api_version != REVIEW_API_VERSION
            || !self.limits.is_valid()
            || !token(&self.request_id)
        {
            return Err("invalid review request");
        }
        if let ReviewOperation::FileDiff { snapshot, file_id } = &self.operation {
            if !token(snapshot) || !token(file_id) {
                return Err("invalid review request identity");
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewFileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    Untracked,
    Unmerged,
    TypeChanged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewContentKind {
    Text,
    Binary,
    Submodule,
    Unreadable,
    Oversized,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewFile {
    pub file_id: String,
    pub old_path: Option<String>,
    pub new_path: Option<String>,
    pub status: ReviewFileStatus,
    pub content_kind: ReviewContentKind,
    pub additions: Option<u64>,
    pub deletions: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewFileList {
    pub snapshot: String,
    pub comparison_label: String,
    pub files: Vec<ReviewFile>,
    /// None means the producer cannot determine the number omitted.
    pub omitted_files: Option<usize>,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewDiff {
    pub snapshot: String,
    pub file_id: String,
    pub content_kind: ReviewContentKind,
    pub hunks: Vec<ReviewHunk>,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewHunk {
    pub old_start: u64,
    pub old_count: u64,
    pub new_start: u64,
    pub new_count: u64,
    pub heading: String,
    pub lines: Vec<ReviewLine>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "text", rename_all = "snake_case")]
pub enum ReviewLine {
    Context(String),
    Addition(String),
    Deletion(String),
    NoNewline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewErrorCode {
    Unsupported,
    StaleSnapshot,
    Unavailable,
    InvalidRequest,
    LimitExceeded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewResponse {
    pub api_version: u32,
    pub request_id: String,
    #[serde(flatten)]
    pub result: ReviewResult,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", content = "data", rename_all = "snake_case")]
pub enum ReviewResult {
    Files(ReviewFileList),
    Diff(ReviewDiff),
    Error {
        code: ReviewErrorCode,
        message: String,
    },
}

// ^ docs/worktree-review.md: decoding alone does not validate provider output.
impl ReviewResponse {
    pub fn validate_for(&self, request: &ReviewRequest) -> Result<(), &'static str> {
        if request.validate().is_err()
            || self.api_version != REVIEW_API_VERSION
            || self.request_id != request.request_id
        {
            return Err("invalid review envelope");
        }
        match (&request.operation, &self.result) {
            (_, ReviewResult::Error { message, .. }) if text(message) => Ok(()),
            (ReviewOperation::ListFiles, ReviewResult::Files(list)) => {
                if !token(&list.snapshot)
                    || !text(&list.comparison_label)
                    || list.files.len() > request.limits.files
                    || (!list.truncated && list.omitted_files != Some(0))
                {
                    return Err("invalid file list");
                }
                let mut ids = std::collections::HashSet::new();
                for file in &list.files {
                    if !token(&file.file_id)
                        || !ids.insert(&file.file_id)
                        || (file.old_path.is_none() && file.new_path.is_none())
                        || file
                            .old_path
                            .iter()
                            .chain(file.new_path.iter())
                            .any(|p| !relative_path(p))
                    {
                        return Err("invalid file identity or path");
                    }
                }
                Ok(())
            }
            (ReviewOperation::FileDiff { snapshot, file_id }, ReviewResult::Diff(diff)) => {
                if !token(snapshot)
                    || !token(file_id)
                    || diff.snapshot != *snapshot
                    || diff.file_id != *file_id
                    || diff.hunks.len() > request.limits.hunks
                    || (diff.content_kind != ReviewContentKind::Text && !diff.hunks.is_empty())
                {
                    return Err("invalid diff identity or kind");
                }
                let mut total = 0usize;
                for hunk in &diff.hunks {
                    total = total
                        .checked_add(hunk.lines.len())
                        .ok_or("too many lines")?;
                    if total > request.limits.lines
                        || !text(&hunk.heading)
                        || hunk.old_start.checked_add(hunk.old_count).is_none()
                        || hunk.new_start.checked_add(hunk.new_count).is_none()
                    {
                        return Err("invalid hunk bounds");
                    }
                    let (mut old, mut new) = (0u64, 0u64);
                    let mut previous_content = false;
                    for line in &hunk.lines {
                        match line {
                            ReviewLine::Context(value) if line_text(value) => {
                                old += 1;
                                new += 1;
                            }
                            ReviewLine::Addition(value) if line_text(value) => new += 1,
                            ReviewLine::Deletion(value) if line_text(value) => old += 1,
                            ReviewLine::NoNewline if previous_content => {}
                            _ => return Err("invalid diff line"),
                        }
                        previous_content = !matches!(line, ReviewLine::NoNewline);
                    }
                    // Truncation drops whole hunks, never silently incomplete ranges.
                    if old != hunk.old_count || new != hunk.new_count {
                        return Err("hunk ranges do not match lines");
                    }
                }
                Ok(())
            }
            _ => Err("unexpected review result"),
        }
    }
}

fn text(value: &str) -> bool {
    value.len() <= 4096 && !value.chars().any(char::is_control)
}

fn line_text(value: &str) -> bool {
    value.len() <= 4096 && !value.chars().any(|c| c.is_control() && c != '\t')
}

fn token(value: &str) -> bool {
    !value.is_empty() && value.len() <= 512 && text(value)
}

fn relative_path(value: &str) -> bool {
    !value.is_empty()
        && text(value)
        && std::path::Path::new(value)
            .components()
            .all(|part| matches!(part, std::path::Component::Normal(_)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_stale_snapshot_is_an_error_not_an_empty_diff() {
        let response: ReviewResponse = serde_json::from_str(
            r#"{"api_version":1,"request_id":"r1","result":"error","data":{"code":"stale_snapshot","message":"refresh required"}}"#,
        ).unwrap();
        assert!(matches!(
            response.result,
            ReviewResult::Error {
                code: ReviewErrorCode::StaleSnapshot,
                ..
            }
        ));
    }

    #[test]
    fn raw_diff_preserves_no_newline_and_zero_length_ranges() {
        let diff: ReviewDiff = serde_json::from_str(
            r#"{"snapshot":"s1","file_id":"f1","content_kind":"text","truncated":false,"hunks":[{"old_start":0,"old_count":0,"new_start":1,"new_count":1,"heading":"","lines":[{"kind":"addition","text":"hello"},{"kind":"no_newline"}]}]}"#,
        ).unwrap();
        assert_eq!(diff.hunks[0].old_count, 0);
        assert_eq!(diff.hunks[0].lines[1], ReviewLine::NoNewline);
    }

    #[test]
    fn response_validation_rejects_mismatched_identity_and_hunk_counts() {
        let request = ReviewRequest {
            api_version: 1,
            request_id: "r1".into(),
            worktree_id: WorktreeId(1),
            worktree_path: "/repo".into(),
            comparison: ReviewComparison::WorkingAgainstHead,
            limits: ReviewLimits::default(),
            operation: ReviewOperation::FileDiff {
                snapshot: "s1".into(),
                file_id: "f1".into(),
            },
        };
        let mut response = ReviewResponse {
            api_version: 1,
            request_id: "r1".into(),
            result: ReviewResult::Diff(ReviewDiff {
                snapshot: "s1".into(),
                file_id: "f1".into(),
                content_kind: ReviewContentKind::Text,
                truncated: false,
                hunks: vec![ReviewHunk {
                    old_start: 0,
                    old_count: 0,
                    new_start: 1,
                    new_count: 1,
                    heading: String::new(),
                    lines: vec![ReviewLine::Addition("hello".into())],
                }],
            }),
        };
        assert!(response.validate_for(&request).is_ok());
        response.request_id = "other".into();
        assert!(response.validate_for(&request).is_err());
        response.request_id = "r1".into();
        if let ReviewResult::Diff(diff) = &mut response.result {
            diff.hunks[0].new_count = 2;
        }
        assert!(response.validate_for(&request).is_err());
    }

    #[test]
    fn paths_and_terminal_controls_fail_closed() {
        for path in [
            "/etc/passwd",
            "../secret",
            "src/../../secret",
            "bad\u{1b}[31m",
            "",
        ] {
            assert!(!relative_path(path), "{path:?}");
        }
        assert!(relative_path("src/a file.rs"));
        assert!(line_text("\tindent"));
        assert!(!line_text("escape\u{1b}[31m"));
        assert!(!line_text("two\nlines"));
    }

    #[test]
    fn limits_reject_zero_and_excessive_requests() {
        assert!(ReviewLimits::default().is_valid());
        assert!(!ReviewLimits {
            files: 0,
            ..ReviewLimits::default()
        }
        .is_valid());
        assert!(!ReviewLimits {
            hunks: 257,
            ..ReviewLimits::default()
        }
        .is_valid());
        assert!(!ReviewLimits {
            lines: 10_001,
            ..ReviewLimits::default()
        }
        .is_valid());
    }

    #[test]
    fn requests_reject_controls_and_unbounded_operation_tokens() {
        let mut request = ReviewRequest {
            api_version: REVIEW_API_VERSION,
            request_id: "request-1".into(),
            worktree_id: WorktreeId(1),
            worktree_path: "/daemon/resolved".into(),
            comparison: ReviewComparison::WorkingAgainstHead,
            limits: ReviewLimits::default(),
            operation: ReviewOperation::FileDiff {
                snapshot: "snapshot-1".into(),
                file_id: "file-1".into(),
            },
        };
        assert!(request.validate().is_ok());
        request.request_id = "bad\nrequest".into();
        assert!(request.validate().is_err());
        request.request_id = "request-1".into();
        request.operation = ReviewOperation::FileDiff {
            snapshot: "s".repeat(513),
            file_id: "file-1".into(),
        };
        assert!(request.validate().is_err());
    }
}
