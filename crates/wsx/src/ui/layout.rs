use ratatui::prelude::Rect;

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
    pub fn new(area: Rect) -> Self {
        let breadcrumb_height = area.height.min(1);
        Self {
            breadcrumb: Rect::new(area.x, area.y, area.width, breadcrumb_height),
            viewport: Rect::new(
                area.x,
                area.y.saturating_add(breadcrumb_height),
                area.width,
                area.height.saturating_sub(breadcrumb_height),
            ),
        }
    }
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
    fn terminal_breadcrumb_is_part_of_content_not_global_chrome() {
        let layout = TerminalLayout::new(Rect::new(0, 1, 56, 14));

        assert_eq!(layout.breadcrumb, Rect::new(0, 1, 56, 1));
        assert_eq!(layout.viewport, Rect::new(0, 2, 56, 13));
    }
}
