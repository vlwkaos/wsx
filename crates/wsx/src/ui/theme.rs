use ratatui::style::Color;

// ^ Semantic wsx chrome. Terminal ANSI colors remain owned by ui/ansi.rs.
pub const BACKGROUND: Color = Color::Rgb(24, 24, 37);
pub const PANEL: Color = Color::Rgb(30, 30, 46);
pub const PANEL_ACTIVE: Color = Color::Rgb(36, 36, 54);
pub const ROW_SELECTED: Color = Color::Rgb(69, 71, 90);
pub const ROW_MOVE: Color = Color::Rgb(49, 82, 72);
pub const TEXT: Color = Color::Rgb(205, 214, 244);
pub const TEXT_MUTED: Color = Color::Rgb(127, 132, 156);
pub const TEXT_SUBTLE: Color = Color::Rgb(108, 112, 134);
pub const ACCENT: Color = Color::Rgb(137, 180, 250);
pub const SUCCESS: Color = Color::Rgb(166, 227, 161);
pub const WORKING: Color = Color::Rgb(249, 226, 175);
pub const DONE: Color = Color::Rgb(148, 226, 213);
pub const BLOCKED: Color = Color::Rgb(243, 139, 168);
pub const UNKNOWN: Color = Color::Rgb(108, 112, 134);
pub const WARNING: Color = Color::Rgb(250, 179, 135);
pub const ERROR: Color = Color::Rgb(243, 139, 168);
pub const DIVIDER: Color = Color::Rgb(49, 50, 68);
pub const TOAST_BACKGROUND: Color = Color::Rgb(49, 50, 68);
