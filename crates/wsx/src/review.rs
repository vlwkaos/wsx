//! Host-owned review focus and asynchronous requests. See docs/worktree-review.md.
use crate::action::Action;
use std::{path::PathBuf, sync::mpsc};
use wsx_core::runtime::*;

pub struct ReviewState {
    pub path: PathBuf,
    pub files: Option<ReviewFileList>,
    pub selected: usize,
    pub diff: Option<ReviewDiff>,
    pub diff_focus: bool,
    pub scroll: usize,
    pub message: String,
    client: Client,
    plugin: Option<String>,
    worktree: Option<WorktreeId>,
    epoch: Option<u64>,
    pending: Option<mpsc::Receiver<Result<Loaded, String>>>,
    request_id: Option<String>,
    check_only: bool,
    last_check: std::time::Instant,
}

impl Drop for ReviewState {
    fn drop(&mut self) {
        if let Some(request_id) = self.request_id.take() {
            let client = self.client.clone();
            std::thread::spawn(move || {
                let _ = client.call(&Request::PluginReviewCancel { request_id });
            });
        }
    }
}

enum Loaded {
    Files(String, WorktreeId, u64, ReviewFileList),
    Diff(u64, ReviewDiff),
}

impl ReviewState {
    pub fn new(path: PathBuf, client: Client) -> Self {
        let mut state = Self {
            path,
            files: None,
            selected: 0,
            diff: None,
            diff_focus: false,
            scroll: 0,
            message: "Loading changes".into(),
            client,
            plugin: None,
            worktree: None,
            epoch: None,
            pending: None,
            request_id: None,
            check_only: false,
            last_check: std::time::Instant::now(),
        };
        state.refresh();
        state
    }

    fn refresh(&mut self) {
        if self.pending.is_some() {
            return;
        }
        let (tx, rx) = mpsc::sync_channel(1);
        let client = self.client.clone();
        let path = self.path.clone();
        let request_id = format!("review-{}", new_client_id());
        self.request_id = Some(request_id.clone());
        self.last_check = std::time::Instant::now();
        if !self.check_only {
            self.message = "Loading changes".into();
        }
        self.pending = Some(rx);
        std::thread::spawn(move || {
            let result = (|| {
                let Response::Plugins(mut plugins) = client
                    .call(&Request::PluginList)
                    .map_err(|e| e.to_string())?
                else {
                    return Err("Cannot discover review providers".into());
                };
                plugins.retain(|p| {
                    p.enabled
                        && p.worktree_review.as_ref().is_some_and(|s| {
                            s.api_version == REVIEW_API_VERSION
                                && s.comparisons
                                    .contains(&ReviewComparison::WorkingAgainstHead)
                        })
                });
                plugins.sort_by_key(|p| {
                    (
                        std::cmp::Reverse(p.worktree_review.as_ref().map_or(0, |s| s.priority)),
                        p.id.clone(),
                    )
                });
                let plugin = plugins
                    .first()
                    .ok_or("No worktree review provider installed")?;
                let Response::Snapshot(snapshot) =
                    client.call(&Request::Snapshot).map_err(|e| e.to_string())?
                else {
                    return Err("Cannot resolve worktree".into());
                };
                let worktree = snapshot
                    .worktrees
                    .iter()
                    .find(|w| w.path == path)
                    .ok_or("Worktree no longer exists")?;
                match client
                    .call(&Request::PluginReview {
                        plugin_id: plugin.id.clone(),
                        worktree_id: worktree.id,
                        request_id,
                        comparison: ReviewComparison::WorkingAgainstHead,
                        limits: ReviewLimits::default(),
                        operation: ReviewOperation::ListFiles,
                    })
                    .map_err(|e| e.to_string())?
                {
                    Response::PluginReview {
                        epoch,
                        response:
                            ReviewResponse {
                                result: ReviewResult::Files(files),
                                ..
                            },
                    } => Ok(Loaded::Files(plugin.id.clone(), worktree.id, epoch, files)),
                    Response::PluginReview {
                        response:
                            ReviewResponse {
                                result: ReviewResult::Error { message, .. },
                                ..
                            },
                        ..
                    } => Err(message),
                    Response::Error(error) => Err(error.message),
                    _ => Err("Unexpected review response".into()),
                }
            })();
            let _ = tx.send(result);
        });
    }

    fn load_diff(&mut self) {
        if self.pending.is_some() {
            return;
        }
        let Some(list) = &self.files else {
            return;
        };
        let Some(file) = list.files.get(self.selected) else {
            return;
        };
        let (Some(plugin), Some(worktree)) = (&self.plugin, self.worktree) else {
            return;
        };
        let request_id = format!("review-{}", new_client_id());
        self.request_id = Some(request_id.clone());
        let request = Request::PluginReview {
            plugin_id: plugin.clone(),
            worktree_id: worktree,
            request_id,
            comparison: ReviewComparison::WorkingAgainstHead,
            limits: ReviewLimits::default(),
            operation: ReviewOperation::FileDiff {
                snapshot: list.snapshot.clone(),
                file_id: file.file_id.clone(),
            },
        };
        let client = self.client.clone();
        let (tx, rx) = mpsc::sync_channel(1);
        self.pending = Some(rx);
        self.message = "Loading diff".into();
        std::thread::spawn(move || {
            let result = match client.call(&request) {
                Ok(Response::PluginReview {
                    epoch,
                    response:
                        ReviewResponse {
                            result: ReviewResult::Diff(diff),
                            ..
                        },
                }) => Ok(Loaded::Diff(epoch, diff)),
                Ok(Response::PluginReview {
                    response:
                        ReviewResponse {
                            result: ReviewResult::Error { message, .. },
                            ..
                        },
                    ..
                }) => Err(message),
                Ok(Response::Error(error)) => Err(error.message),
                Err(error) => Err(error.to_string()),
                _ => Err("Unexpected review response".into()),
            };
            let _ = tx.send(result);
        });
    }

    pub fn poll(&mut self) -> bool {
        if self.pending.is_none()
            && self.files.is_some()
            && self.last_check.elapsed() >= std::time::Duration::from_secs(3)
        {
            self.check_only = true;
            self.refresh();
        }
        let Some(receiver) = &self.pending else {
            return false;
        };
        let result = match receiver.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return false,
            Err(_) => Err("Review worker stopped".into()),
        };
        self.pending = None;
        self.request_id = None;
        match result {
            Ok(Loaded::Files(plugin, worktree, epoch, list)) => {
                if self.check_only {
                    self.check_only = false;
                    if self.epoch != Some(epoch)
                        || self.plugin.as_ref() != Some(&plugin)
                        || self
                            .files
                            .as_ref()
                            .is_some_and(|old| old.snapshot != list.snapshot)
                    {
                        self.message = "Changes updated (r) refresh".into();
                    }
                    return true;
                }
                let old_id = self
                    .files
                    .as_ref()
                    .and_then(|l| l.files.get(self.selected))
                    .map(|f| f.file_id.clone());
                self.selected = old_id
                    .and_then(|id| list.files.iter().position(|f| f.file_id == id))
                    .unwrap_or(0);
                self.files = Some(list);
                self.plugin = Some(plugin);
                self.worktree = Some(worktree);
                self.epoch = Some(epoch);
                self.diff = None;
                self.scroll = 0;
                self.message.clear();
                self.load_diff();
            }
            Ok(Loaded::Diff(epoch, diff)) => {
                let valid = self.epoch == Some(epoch)
                    && self.files.as_ref().is_some_and(|list| {
                        list.snapshot == diff.snapshot
                            && list
                                .files
                                .get(self.selected)
                                .is_some_and(|f| f.file_id == diff.file_id)
                    });
                if valid {
                    self.diff = Some(diff);
                    self.message.clear();
                } else {
                    self.message = "Changes updated (r) refresh".into();
                }
            }
            Err(error) => self.message = error,
        }
        true
    }

    /// False exits review. While a request is pending, keep selection stable.
    pub fn handle(&mut self, action: &Action) -> bool {
        match action {
            Action::InputEscape if self.diff_focus => self.diff_focus = false,
            Action::InputEscape => return false,
            Action::Select if self.diff.is_some() => self.diff_focus = true,
            Action::SetAlias | Action::Refresh if self.pending.is_none() => {
                self.check_only = false;
                self.refresh();
            }
            Action::PageDown if self.diff_focus => self.scroll = self.scroll.saturating_add(10),
            Action::PageUp if self.diff_focus => self.scroll = self.scroll.saturating_sub(10),
            Action::JumpProjectDown | Action::JumpProjectUp if self.diff_focus => {
                let mut offset = 0;
                let starts = self
                    .diff
                    .iter()
                    .flat_map(|diff| diff.hunks.iter())
                    .map(|hunk| {
                        let start = offset;
                        offset += 1 + hunk.lines.len();
                        start
                    })
                    .collect::<Vec<_>>();
                self.scroll = if matches!(action, Action::JumpProjectDown) {
                    starts
                        .into_iter()
                        .find(|start| *start > self.scroll)
                        .unwrap_or(self.scroll)
                } else {
                    starts
                        .into_iter()
                        .rev()
                        .find(|start| *start < self.scroll)
                        .unwrap_or(0)
                };
            }
            Action::NavigateDown if self.diff_focus => self.scroll = self.scroll.saturating_add(1),
            Action::NavigateUp if self.diff_focus => self.scroll = self.scroll.saturating_sub(1),
            Action::NavigateDown | Action::NavigateUp if self.pending.is_none() => {
                let count = self.files.as_ref().map_or(0, |l| l.files.len());
                let old = self.selected;
                if matches!(action, Action::NavigateDown) {
                    self.selected = (old + 1).min(count.saturating_sub(1));
                } else {
                    self.selected = old.saturating_sub(1);
                }
                if old != self.selected {
                    self.diff = None;
                    self.scroll = 0;
                    self.load_diff();
                }
            }
            _ => {}
        }
        if let Some(diff) = &self.diff {
            let rows = diff.hunks.iter().map(|h| h.lines.len() + 1).sum::<usize>();
            self.scroll = self.scroll.min(rows.saturating_sub(1));
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> ReviewState {
        ReviewState {
            path: "/repo".into(),
            files: None,
            selected: 0,
            diff: Some(ReviewDiff {
                snapshot: "s".into(),
                file_id: "f".into(),
                content_kind: ReviewContentKind::Text,
                truncated: false,
                hunks: vec![
                    ReviewHunk {
                        old_start: 0,
                        old_count: 0,
                        new_start: 1,
                        new_count: 1,
                        heading: String::new(),
                        lines: vec![ReviewLine::Addition("text".into())]
                    };
                    2
                ],
            }),
            diff_focus: false,
            scroll: 0,
            message: String::new(),
            client: Client::new("/nonexistent"),
            plugin: None,
            worktree: None,
            epoch: None,
            pending: None,
            request_id: None,
            check_only: false,
            last_check: std::time::Instant::now(),
        }
    }

    #[test]
    fn updated_snapshot_does_not_replace_the_diff_being_read() {
        let mut review = state();
        review.epoch = Some(1);
        review.plugin = Some("git".into());
        review.files = Some(ReviewFileList {
            snapshot: "old".into(),
            comparison_label: "HEAD".into(),
            files: vec![],
            omitted_files: Some(0),
            truncated: false,
        });
        review.check_only = true;
        review.scroll = 2;
        let original = review.diff.clone();
        let (tx, rx) = mpsc::sync_channel(1);
        tx.send(Ok(Loaded::Files(
            "git".into(),
            WorktreeId(1),
            1,
            ReviewFileList {
                snapshot: "new".into(),
                comparison_label: "HEAD".into(),
                files: vec![],
                omitted_files: Some(0),
                truncated: false,
            },
        )))
        .unwrap();
        review.pending = Some(rx);
        assert!(review.poll());
        assert_eq!(review.diff, original);
        assert_eq!(review.scroll, 2);
        assert!(review.message.contains("Changes updated"));
        assert_eq!(review.files.as_ref().unwrap().snapshot, "old");
    }

    #[test]
    fn focus_unwinds_and_hunks_scroll_without_changing_tree() {
        let mut review = state();
        assert!(review.handle(&Action::Select));
        assert!(review.diff_focus);
        review.handle(&Action::JumpProjectDown);
        assert_eq!(review.scroll, 2);
        review.handle(&Action::JumpProjectUp);
        assert_eq!(review.scroll, 0);
        assert!(review.handle(&Action::InputEscape));
        assert!(!review.diff_focus);
        assert!(!review.handle(&Action::InputEscape));
    }

    #[test]
    fn review_renders_normal_and_tiny_without_mutating_scroll() {
        use ratatui::{backend::TestBackend, Terminal};
        for (width, height) in [(100, 30), (30, 8), (1, 1), (0, 0)] {
            let mut review = state();
            review.diff_focus = true;
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| {
                    crate::ui::review::render(
                        frame,
                        frame.area(),
                        vec![
                            ratatui::text::Line::from("Branch: main"),
                            ratatui::text::Line::from("Path: /repo"),
                        ],
                        &review,
                    )
                })
                .unwrap();
            assert_eq!(review.scroll, 0);
            if width == 100 {
                let text = terminal
                    .backend()
                    .buffer()
                    .content()
                    .iter()
                    .map(|c| c.symbol())
                    .collect::<String>();
                assert!(text.contains("+text"));
                assert!(text.contains("Branch:"));
            }
        }
    }
}
