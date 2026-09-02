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

    /// ^ Stable navigation frame. Gutters stay reserved when focus border
    /// glyphs are hidden, so rows do not move between Workspace and Terminal.
    pub fn bordered(area: Rect) -> Self {
        let mut layout = Self::from_area(area, false);
        layout.list.y = layout.list.y.saturating_add(1);
        layout.list.height = layout.list.height.saturating_sub(2);
        layout.scrollbar.y = layout.scrollbar.y.saturating_add(1);
        layout.scrollbar.height = layout.scrollbar.height.saturating_sub(2);
        layout
    }

    /// One data cell plus a divider, with the same vertical gutters as the expanded tree.
    pub fn compact_rail(area: Rect) -> Self {
        Self {
            header: Rect::default(),
            list: Rect::new(
                area.x,
                area.y.saturating_add(1),
                area.width.min(1),
                area.height.saturating_sub(2),
            ),
            scrollbar: Rect::default(),
        }
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
    active_group: &GroupKey,
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

    let all_width = cursor
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
            active: active_group == key,
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
                active: active_group == key,
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

fn scrollable_positions(content_len: usize, viewport_len: usize) -> usize {
    if content_len > viewport_len {
        content_len - viewport_len + 1
    } else {
        0
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
    // ^ [[wsx UI Patterns]] Ratatui adds the viewport length when sizing the thumb,
    // so its content length is the number of valid top-row positions, not total rows.
    let mut state = ScrollbarState::new(scrollable_positions(content_len, viewport_len))
        .position(position)
        .viewport_content_length(viewport_len);
    frame.render_stateful_widget(scrollbar, area, &mut state);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn groups() -> Vec<GroupKey> {
        vec![
            GroupKey::Ungrouped,
            GroupKey::Named("personal".into()),
            GroupKey::Named("work".into()),
        ]
    }

    #[test]
    fn bordered_sidebar_keeps_stable_content_gutters() {
        let area = Rect::new(4, 5, 32, 10);
        let plain = SidebarLayout::new(area);
        let bordered = SidebarLayout::bordered(area);

        assert_eq!(
            (bordered.list.x, bordered.list.width),
            (plain.list.x, plain.list.width)
        );
        assert_eq!(bordered.scrollbar.x, plain.scrollbar.x);
        assert_eq!((bordered.list.y, bordered.list.height), (6, 8));
        assert_eq!(bordered.item_at(Position::new(5, 5), 0, 20), None);
        assert_eq!(bordered.item_at(Position::new(5, 6), 3, 20), Some(3));

        for height in 0..=2 {
            let tiny = SidebarLayout::bordered(Rect::new(0, 0, 1, height));
            assert_eq!(tiny.list.height, 0);
            assert_eq!(tiny.item_at(Position::new(0, 0), 0, 1), None);
        }
    }

    #[test]
    fn compact_rail_preserves_expanded_tree_row_coordinates() {
        let layout = SidebarLayout::compact_rail(Rect::new(4, 5, 2, 10));

        assert_eq!(layout.list, Rect::new(4, 6, 1, 8));
        assert_eq!(layout.scrollbar, Rect::default());
        assert_eq!(layout.item_at(Position::new(4, 5), 3, 20), None);
        assert_eq!(layout.item_at(Position::new(4, 6), 3, 20), Some(3));
        assert_eq!(layout.item_at(Position::new(5, 6), 3, 20), None);

        for height in 0..=2 {
            let tiny = SidebarLayout::compact_rail(Rect::new(0, 0, 2, height));
            assert_eq!(tiny.list.height, 0);
            assert_eq!(tiny.item_at(Position::new(0, 0), 0, 1), None);
        }
    }

    #[test]
    fn wide_projection_shows_ungrouped_and_named_groups() {
        let strip = fit_group_strip(&groups(), &GroupKey::Ungrouped, 100, 0);
        assert_eq!(strip.chips.len(), 3);
        assert!(strip.chips[0].active);
        assert!(strip.left_cells.is_none());
        assert!(strip.right_cells.is_none());
    }

    #[test]
    fn narrow_projection_scrolls_by_chip_with_exact_boundaries() {
        let strip = fit_group_strip(&groups(), &GroupKey::Ungrouped, 28, 0);
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
        let shifted = fit_group_strip(&groups(), &GroupKey::Named("work".into()), 28, 2);
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
        let strip = fit_group_strip(&unicode, &unicode[0], 18, 0);
        assert_eq!(strip.chips.len(), 1);
        assert!(strip.chips[0].cells.end <= 18);
        assert!(fit_group_strip(&unicode, &GroupKey::Ungrouped, 0, 0)
            .chips
            .is_empty());
    }

    #[test]
    fn scrollbar_content_length_tracks_valid_rendered_row_positions() {
        assert_eq!(scrollable_positions(100, 20), 81);
        assert_eq!(scrollable_positions(21, 20), 2);
        assert_eq!(scrollable_positions(20, 20), 0);
        assert_eq!(scrollable_positions(0, 0), 0);
    }

    #[test]
    fn scrollbar_thumb_height_matches_visible_fraction_of_rendered_rows() {
        let backend = ratatui::backend::TestBackend::new(1, 20);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| render_scrollbar(frame, frame.area(), 100, 20, 0))
            .unwrap();

        let thumb_height = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .filter(|cell| cell.symbol() == "█")
            .count();
        assert_eq!(thumb_height, 4);
    }
}
