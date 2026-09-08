use ratatui::{
    layout::{Constraint, Direction, Layout},
    prelude::Rect,
};
use wsx_core::config::global::{TerminalSidebar, TerminalTitlePosition};

pub const EXPANDED_SIDEBAR_WIDTH: u16 = 32;
pub const COMPACT_SIDEBAR_WIDTH: u16 = 2;

pub fn terminal_sidebar_width(sidebar: TerminalSidebar) -> u16 {
    match sidebar {
        TerminalSidebar::Compact => COMPACT_SIDEBAR_WIDTH,
        TerminalSidebar::Expanded => EXPANDED_SIDEBAR_WIDTH,
    }
}

// ^ [[wsx UI Patterns]] Global chrome and terminal breadcrumb geometry share one contract.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameLayout {
    pub header: Rect,
    pub content: Rect,
    pub footer: Rect,
}

impl FrameLayout {
    pub fn new(area: Rect) -> Self {
        let footer_height = area.height.min(1);
        let header_height = area.height.saturating_sub(footer_height).min(1);
        let content_height = area.height.saturating_sub(header_height + footer_height);

        Self {
            header: Rect::new(area.x, area.y, area.width, header_height),
            content: Rect::new(
                area.x,
                area.y.saturating_add(header_height),
                area.width,
                content_height,
            ),
            footer: Rect::new(
                area.x,
                area.bottom().saturating_sub(footer_height),
                area.width,
                footer_height,
            ),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalLayout {
    pub breadcrumb: Rect,
    pub viewport: Rect,
}

impl TerminalLayout {
    pub fn new(area: Rect, title_position: TerminalTitlePosition) -> Self {
        let title_height = area.height.min(1);
        let viewport_height = area.height.saturating_sub(title_height);
        let (title_y, viewport_y) = match title_position {
            TerminalTitlePosition::Top => (area.y, area.y.saturating_add(title_height)),
            TerminalTitlePosition::Bottom => (area.y.saturating_add(viewport_height), area.y),
        };
        Self {
            breadcrumb: Rect::new(area.x, title_y, area.width, title_height),
            viewport: Rect::new(area.x, viewport_y, area.width, viewport_height),
        }
    }
}

/// Compute the terminal viewport directly from the outer terminal size.
/// Subscription must not depend on geometry cached by a previous Workspace frame.
pub fn terminal_viewport(
    area: Rect,
    mobile: bool,
    sidebar: TerminalSidebar,
    title_position: TerminalTitlePosition,
) -> Rect {
    let content = FrameLayout::new(area).content;
    let panel = if mobile {
        content
    } else {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(terminal_sidebar_width(sidebar)),
                Constraint::Min(0),
            ])
            .split(content)[1]
    };
    TerminalLayout::new(panel, title_position).viewport
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_has_one_header_and_one_footer_without_a_spacer() {
        let layout = FrameLayout::new(Rect::new(3, 5, 80, 10));

        assert_eq!(layout.header, Rect::new(3, 5, 80, 1));
        assert_eq!(layout.content, Rect::new(3, 6, 80, 8));
        assert_eq!(layout.footer, Rect::new(3, 14, 80, 1));
    }

    #[test]
    fn frame_prioritizes_footer_then_header_at_tiny_heights() {
        assert_eq!(
            FrameLayout::new(Rect::new(0, 0, 20, 1)),
            FrameLayout {
                header: Rect::new(0, 0, 20, 0),
                content: Rect::new(0, 0, 20, 0),
                footer: Rect::new(0, 0, 20, 1),
            }
        );
        assert_eq!(
            FrameLayout::new(Rect::new(0, 0, 20, 2)),
            FrameLayout {
                header: Rect::new(0, 0, 20, 1),
                content: Rect::new(0, 1, 20, 0),
                footer: Rect::new(0, 1, 20, 1),
            }
        );
    }

    #[test]
    fn subscription_viewport_is_derived_without_prior_render_state() {
        assert_eq!(
            terminal_viewport(
                Rect::new(0, 0, 80, 24),
                true,
                TerminalSidebar::Compact,
                TerminalTitlePosition::Bottom,
            ),
            Rect::new(0, 1, 80, 21)
        );
        assert_eq!(
            terminal_viewport(
                Rect::new(0, 0, 80, 24),
                false,
                TerminalSidebar::Compact,
                TerminalTitlePosition::Bottom,
            ),
            Rect::new(2, 1, 78, 21)
        );
        assert_eq!(
            terminal_viewport(
                Rect::new(0, 0, 80, 24),
                false,
                TerminalSidebar::Expanded,
                TerminalTitlePosition::Bottom,
            ),
            Rect::new(32, 1, 48, 21)
        );
    }

    #[test]
    fn compact_terminal_viewport_stays_bounded_at_zero_and_one_column() {
        for width in 0..=2 {
            let area = Rect::new(3, 5, width, 4);
            let viewport = terminal_viewport(
                area,
                false,
                TerminalSidebar::Compact,
                TerminalTitlePosition::Bottom,
            );
            assert!(viewport.x >= area.x);
            assert!(viewport.right() <= area.right());
            assert!(viewport.y >= area.y);
            assert!(viewport.bottom() <= area.bottom());
        }
    }

    #[test]
    fn terminal_title_moves_without_changing_viewport_size() {
        let area = Rect::new(0, 1, 56, 14);
        let top = TerminalLayout::new(area, TerminalTitlePosition::Top);
        let bottom = TerminalLayout::new(area, TerminalTitlePosition::Bottom);

        assert_eq!(top.breadcrumb, Rect::new(0, 1, 56, 1));
        assert_eq!(top.viewport, Rect::new(0, 2, 56, 13));
        assert_eq!(bottom.viewport, Rect::new(0, 1, 56, 13));
        assert_eq!(bottom.breadcrumb, Rect::new(0, 14, 56, 1));
        assert_eq!(top.viewport.as_size(), bottom.viewport.as_size());
    }
}
