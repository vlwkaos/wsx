use ratatui::style::Color;

// ^ Semantic wsx chrome. Terminal ANSI colors remain owned by ui/ansi.rs.
pub const BACKGROUND: Color = Color::Rgb(8, 9, 11);
pub const PANEL: Color = Color::Rgb(14, 16, 20);
pub const PANEL_ACTIVE: Color = Color::Rgb(19, 22, 28);
pub const ROW_SELECTED: Color = Color::Rgb(36, 43, 55);
pub const ROW_MOVE: Color = Color::Rgb(27, 58, 49);
pub const TEXT: Color = Color::Rgb(224, 229, 237);
pub const TEXT_MUTED: Color = Color::Rgb(143, 152, 166);
pub const TEXT_SUBTLE: Color = Color::Rgb(99, 108, 123);
pub const ACCENT: Color = Color::Rgb(102, 153, 255);
pub const SUCCESS: Color = Color::Rgb(88, 190, 112);
pub const WORKING: Color = Color::Rgb(226, 190, 118);
pub const DONE: Color = Color::Rgb(91, 199, 188);
pub const BLOCKED: Color = Color::Rgb(234, 105, 126);
pub const UNKNOWN: Color = Color::Rgb(99, 108, 123);
pub const WARNING: Color = Color::Rgb(224, 151, 91);
pub const ERROR: Color = Color::Rgb(234, 105, 126);
pub const DIVIDER: Color = Color::Rgb(34, 39, 48);
pub const TOAST_BACKGROUND: Color = Color::Rgb(24, 28, 35);

#[cfg(test)]
mod tests {
    use super::*;

    fn brightness(color: Color) -> u16 {
        match color {
            Color::Rgb(red, green, blue) => red as u16 + green as u16 + blue as u16,
            _ => panic!("semantic theme colors must use explicit RGB values"),
        }
    }

    #[test]
    fn neutral_black_surfaces_and_text_keep_their_visual_hierarchy() {
        for color in [
            BACKGROUND,
            PANEL,
            PANEL_ACTIVE,
            ROW_SELECTED,
            ROW_MOVE,
            TEXT,
            TEXT_MUTED,
            TEXT_SUBTLE,
            ACCENT,
            SUCCESS,
            WORKING,
            DONE,
            BLOCKED,
            UNKNOWN,
            WARNING,
            ERROR,
            DIVIDER,
            TOAST_BACKGROUND,
        ] {
            assert!(matches!(color, Color::Rgb(..)));
        }

        assert!(brightness(BACKGROUND) < brightness(PANEL));
        assert!(brightness(PANEL) < brightness(PANEL_ACTIVE));
        assert!(brightness(PANEL_ACTIVE) < brightness(ROW_SELECTED));
        assert!(brightness(TEXT_SUBTLE) < brightness(TEXT_MUTED));
        assert!(brightness(TEXT_MUTED) < brightness(TEXT));
    }
}
