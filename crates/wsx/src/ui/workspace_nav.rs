use ratatui::{
    prelude::*,
    widgets::{Scrollbar, ScrollbarOrientation, ScrollbarState},
};
use wsx_core::config::global::GroupKey;

use super::theme;

pub const WORKSPACE_HEADER_TITLE: &str = " workspace ";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SidebarLayout {
    pub header: Rect,
    pub list: Rect,
    pub scrollbar: Rect,
}

impl SidebarLayout {
    /// Sidebar content without local chrome; the global Workspace header owns navigation.
    pub fn new(area: Rect) -> Self {
        Self::from_area(area, false)
    }

    /// Sidebar content with a local title and one blank row, used by Group Manager.
    pub fn with_header(area: Rect) -> Self {
        Self::from_area(area, true)
    }

    fn from_area(area: Rect, has_header: bool) -> Self {
        let inner_x = area.x.saturating_add(1);
        let inner_width = area.width.saturating_sub(2);
        let list_y = area.y.saturating_add(if has_header { 2 } else { 0 });
        let list_height = area.height.saturating_sub(if has_header { 2 } else { 0 });
        let scrollbar_x = area.right().saturating_sub(2);
        Self {
            header: Rect::new(inner_x, area.y, inner_width, u16::from(has_header)),
            list: Rect::new(inner_x, list_y, inner_width.saturating_sub(1), list_height),
            scrollbar: Rect::new(scrollbar_x, list_y, u16::from(inner_width > 0), list_height),
        }
    }

    pub fn item_at(self, position: Position, scroll: usize, len: usize) -> Option<usize> {
        if !self.list.contains(position) {
            return None;
        }
        let index = usize::from(position.y - self.list.y).saturating_add(scroll);
        (index < len).then_some(index)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupChip {
    pub key: GroupKey,
    pub label: String,
    pub active: bool,
    /// Half-open display-cell range relative to the complete header row.
    pub cells: std::ops::Range<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupStrip {
    pub chips: Vec<GroupChip>,
    pub scroll_start: usize,
    pub left_cells: Option<std::ops::Range<usize>>,
    pub right_cells: Option<std::ops::Range<usize>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupStripTarget {
    Group(GroupKey),
    ScrollLeft,
    ScrollRight,
}

pub fn group_label(group: &GroupKey) -> &str {
    match group {
        GroupKey::Recent => "◷ recent",
        GroupKey::Ungrouped => "ungrouped",
        GroupKey::Named(name) => name,
    }
}

fn chip_width(label: &str) -> usize {
    Line::from(format!(" {label} ")).width()
}

fn truncate_label(label: &str, max_chip_width: usize) -> String {
    if chip_width(label) <= max_chip_width {
        return label.to_string();
    }
    let mut value = label.to_string();
    while !value.is_empty() && chip_width(&format!("{value}…")) > max_chip_width {
        value.pop();
    }
    if value.is_empty() {
        "…".into()
    } else {
        format!("{value}…")
    }
}

/// Project one horizontally scrollable, whole-chip Workspace header.
// ^ [[wsx UI Patterns]] Render and mouse hit testing consume this exact cell projection.
pub fn fit_group_strip(
    groups: &[GroupKey],
    active_group: Option<&GroupKey>,
    available_width: usize,
    requested_start: usize,
) -> GroupStrip {
    let title_width = Line::from(WORKSPACE_HEADER_TITLE)
        .width()
        .min(available_width);
    let mut cursor = title_width;
    if groups.is_empty() || cursor >= available_width {
        return GroupStrip {
            chips: Vec::new(),
            scroll_start: 0,
            left_cells: None,
            right_cells: None,
        };
    }

    let all_width = title_width
        + groups
            .iter()
            .map(|group| chip_width(group_label(group)))
            .sum::<usize>()
        + groups.len().saturating_sub(1);
    let start = if all_width <= available_width {
        0
    } else {
        requested_start.min(groups.len().saturating_sub(1))
    };
    let left_cells = (start > 0 && cursor < available_width).then(|| {
        let range = cursor..cursor + 1;
        cursor += 2;
        range
    });
    let reserve_right = usize::from(start + 1 < groups.len()) * 2;
    let chip_limit = available_width.saturating_sub(cursor + reserve_right);
    let mut chips = Vec::new();
    let mut used = 0usize;
    for key in groups.iter().skip(start) {
        let separator = usize::from(!chips.is_empty());
        let remaining = chip_limit.saturating_sub(used + separator);
        if remaining < 3 {
            break;
        }
        let label = truncate_label(group_label(key), remaining);
        let width = chip_width(&label);
        if width > remaining {
            break;
        }
        used += separator;
        let cells = (cursor + used)..(cursor + used + width);
        used += width;
        chips.push(GroupChip {
            key: key.clone(),
            label,
            active: active_group == Some(key),
            cells,
        });
    }
    if chips.is_empty() && cursor < available_width {
        let remaining = available_width.saturating_sub(cursor + reserve_right);
        if remaining >= 3 {
            let key = &groups[start];
            let label = truncate_label(group_label(key), remaining);
            let width = chip_width(&label).min(remaining);
            chips.push(GroupChip {
                key: key.clone(),
                label,
                active: active_group == Some(key),
                cells: cursor..cursor + width,
            });
            used = width;
        }
    }
    let shown_end = start + chips.len();
    let right_cells = (shown_end < groups.len()).then(|| {
        let arrow =
            (cursor + used + usize::from(!chips.is_empty())).min(available_width.saturating_sub(1));
        arrow..arrow + 1
    });
    GroupStrip {
        chips,
        scroll_start: start,
        left_cells,
        right_cells,
    }
}

impl GroupStrip {
    pub fn target_at(&self, cell: usize) -> Option<GroupStripTarget> {
        self.chips
            .iter()
            .find(|chip| chip.cells.contains(&cell))
            .map(|chip| GroupStripTarget::Group(chip.key.clone()))
            .or_else(|| {
                self.left_cells
                    .as_ref()
                    .filter(|range| range.contains(&cell))
                    .map(|_| GroupStripTarget::ScrollLeft)
            })
            .or_else(|| {
                self.right_cells
                    .as_ref()
                    .filter(|range| range.contains(&cell))
                    .map(|_| GroupStripTarget::ScrollRight)
            })
    }
}

pub fn render_scrollbar(
    frame: &mut Frame,
    area: Rect,
    content_len: usize,
    viewport_len: usize,
    position: usize,
) {
    if area.width == 0 || area.height == 0 || content_len <= viewport_len {
        return;
    }
    let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .begin_symbol(None)
        .end_symbol(None)
        .track_style(theme::scrollbar_track())
        .thumb_style(theme::scrollbar_thumb());
    let mut state = ScrollbarState::new(content_len)
        .position(position)
        .viewport_content_length(viewport_len);
    frame.render_stateful_widget(scrollbar, area, &mut state);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn groups() -> Vec<GroupKey> {
        vec![
            GroupKey::Recent,
            GroupKey::Ungrouped,
            GroupKey::Named("personal".into()),
            GroupKey::Named("work".into()),
        ]
    }

    #[test]
    fn wide_projection_shows_every_group_without_overflow() {
        let strip = fit_group_strip(&groups(), Some(&GroupKey::Recent), 100, 0);
        assert_eq!(strip.chips.len(), 4);
        assert!(strip.left_cells.is_none());
        assert!(strip.right_cells.is_none());
    }

    #[test]
    fn narrow_projection_scrolls_by_chip_with_exact_boundaries() {
        let strip = fit_group_strip(&groups(), Some(&GroupKey::Recent), 28, 0);
        let right = strip.right_cells.clone().unwrap();
        assert_eq!(
            strip.target_at(right.start),
            Some(GroupStripTarget::ScrollRight)
        );
        let first = &strip.chips[0];
        assert_eq!(
            strip.target_at(first.cells.start),
            Some(GroupStripTarget::Group(first.key.clone()))
        );
        assert_ne!(
            strip.target_at(first.cells.end),
            Some(GroupStripTarget::Group(first.key.clone()))
        );
        let shifted = fit_group_strip(&groups(), Some(&GroupKey::Named("work".into())), 28, 3);
        let left = shifted.left_cells.clone().unwrap();
        assert_eq!(
            shifted.target_at(left.start),
            Some(GroupStripTarget::ScrollLeft)
        );
        assert!(shifted.chips.iter().any(|chip| chip.active));
    }

    #[test]
    fn unicode_labels_use_display_cells_and_tiny_widths_do_not_panic() {
        let unicode = vec![GroupKey::Named("개인-project".into())];
        let strip = fit_group_strip(&unicode, Some(&unicode[0]), 18, 0);
        assert_eq!(strip.chips.len(), 1);
        assert!(strip.chips[0].cells.end <= 18);
        assert!(fit_group_strip(&unicode, None, 0, 0).chips.is_empty());
    }
}
