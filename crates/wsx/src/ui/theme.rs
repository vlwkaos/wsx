use ratatui::{
    prelude::Stylize,
    style::{Color, Style},
};

// ^ Semantic wsx chrome. Explicit terminal-cell ANSI backgrounds remain content-owned.
const BACKGROUND: Color = Color::Rgb(8, 9, 11);
const PANEL: Color = Color::Rgb(14, 16, 20);
const PANEL_ACTIVE: Color = Color::Rgb(19, 22, 28);
const ROW_SELECTED: Color = Color::Rgb(36, 43, 55);
const ROW_MOVE: Color = Color::Rgb(27, 58, 49);
pub const TEXT: Color = Color::Rgb(224, 229, 237);
pub const TEXT_MUTED: Color = Color::Rgb(143, 152, 166);
pub const TEXT_SUBTLE: Color = Color::Rgb(99, 108, 123);
pub const ACCENT: Color = Color::Rgb(102, 153, 255);
const RECENT_ACCENT: Color = Color::Rgb(122, 184, 171);
const RECENT_SURFACE: Color = Color::Rgb(22, 38, 38);
pub const SUCCESS: Color = Color::Rgb(88, 190, 112);
pub const WORKING: Color = Color::Rgb(226, 190, 118);
pub const DONE: Color = Color::Rgb(91, 199, 188);
pub const BLOCKED: Color = Color::Rgb(234, 105, 126);
pub const UNKNOWN: Color = Color::Rgb(99, 108, 123);
pub const WARNING: Color = Color::Rgb(224, 151, 91);
pub const ERROR: Color = Color::Rgb(234, 105, 126);
pub const DIVIDER: Color = Color::Rgb(34, 39, 48);
const TOAST_BACKGROUND: Color = Color::Rgb(24, 28, 35);
const MODE_INPUT: Color = Color::Rgb(150, 126, 224);
const MODE_CONFIG: Color = Color::Rgb(184, 132, 224);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeBadge {
    Navigation,
    Terminal,
    Input,
    Confirm,
    Config,
    Move,
    Info,
    Routine,
}

// ^ [[wsx UI Patterns]] Backgrounds are bounded state affordances, never
// whole-surface defaults. Terminal ANSI backgrounds remain content-owned.
pub fn group_chip(active: bool) -> Style {
    if active {
        Style::default().fg(PANEL).bg(ACCENT).bold()
    } else {
        Style::default().fg(TEXT_MUTED).bg(PANEL_ACTIVE)
    }
}

pub fn recent_group_chip(active: bool) -> Style {
    if active {
        Style::default().fg(PANEL).bg(RECENT_ACCENT).bold()
    } else {
        Style::default().fg(RECENT_ACCENT).bg(RECENT_SURFACE)
    }
}

pub fn group_scroll_control() -> Style {
    Style::default().fg(TEXT_MUTED).bold()
}

pub fn selected_row(move_mode: bool) -> Style {
    Style::default()
        .fg(TEXT)
        .bg(if move_mode { ROW_MOVE } else { ROW_SELECTED })
        .bold()
}

pub fn accent_selection() -> Style {
    Style::default().fg(BACKGROUND).bg(ACCENT)
}

pub fn mode_badge(role: ModeBadge) -> Style {
    let background = match role {
        ModeBadge::Navigation => ACCENT,
        ModeBadge::Terminal => DONE,
        ModeBadge::Input => MODE_INPUT,
        ModeBadge::Confirm => BLOCKED,
        ModeBadge::Config => MODE_CONFIG,
        ModeBadge::Move => WORKING,
        ModeBadge::Info => TEXT_MUTED,
        ModeBadge::Routine => SUCCESS,
    };
    Style::default().fg(BACKGROUND).bg(background).bold()
}

pub fn toast_surface() -> Style {
    Style::default().bg(TOAST_BACKGROUND)
}

pub fn toast_accent(accent: Color) -> Style {
    Style::default().bg(accent)
}

pub fn scrollbar_track() -> Style {
    Style::default().fg(DIVIDER)
}

pub fn scrollbar_thumb() -> Style {
    Style::default().fg(TEXT_MUTED)
}

pub fn agent_label() -> Style {
    Style::default().fg(TEXT_MUTED)
}

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
    fn semantic_palette_and_bounded_background_roles_keep_their_hierarchy() {
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
            RECENT_ACCENT,
            RECENT_SURFACE,
            SUCCESS,
            WORKING,
            DONE,
            BLOCKED,
            UNKNOWN,
            WARNING,
            ERROR,
            DIVIDER,
            TOAST_BACKGROUND,
            MODE_INPUT,
            MODE_CONFIG,
        ] {
            assert!(matches!(color, Color::Rgb(..)));
        }

        assert!(brightness(BACKGROUND) < brightness(PANEL));
        assert!(brightness(PANEL) < brightness(PANEL_ACTIVE));
        assert!(brightness(PANEL_ACTIVE) < brightness(ROW_SELECTED));
        assert!(brightness(TEXT_SUBTLE) < brightness(TEXT_MUTED));
        assert!(brightness(TEXT_MUTED) < brightness(TEXT));
        assert_eq!(Style::default().bg, None);
        assert_eq!(group_chip(true).bg, Some(ACCENT));
        assert_eq!(group_chip(false).bg, Some(PANEL_ACTIVE));
        assert_eq!(recent_group_chip(true).bg, Some(RECENT_ACCENT));
        assert_eq!(recent_group_chip(false).bg, Some(RECENT_SURFACE));
        assert_ne!(recent_group_chip(true), group_chip(true));
        assert_eq!(selected_row(false).bg, Some(ROW_SELECTED));
        assert_eq!(agent_label().fg, Some(TEXT_MUTED));
        assert_eq!(mode_badge(ModeBadge::Navigation).bg, Some(ACCENT));
        assert_eq!(mode_badge(ModeBadge::Terminal).bg, Some(DONE));
        assert_eq!(mode_badge(ModeBadge::Input).bg, Some(MODE_INPUT));
        assert_eq!(mode_badge(ModeBadge::Confirm).bg, Some(BLOCKED));
        assert_eq!(mode_badge(ModeBadge::Config).bg, Some(MODE_CONFIG));
        assert_eq!(mode_badge(ModeBadge::Move).bg, Some(WORKING));
        assert_eq!(mode_badge(ModeBadge::Info).bg, Some(TEXT_MUTED));
        assert_eq!(mode_badge(ModeBadge::Routine).bg, Some(SUCCESS));
    }

    #[test]
    fn chrome_backgrounds_flow_through_semantic_theme_roles() {
        let ui_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ui");
        for entry in std::fs::read_dir(ui_dir).unwrap().filter_map(Result::ok) {
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("rs") {
                continue;
            }
            let source = std::fs::read_to_string(&path).unwrap();
            for (line_number, line) in source.lines().enumerate() {
                if !line.contains(".bg(") || path.file_name().unwrap() == "theme.rs" {
                    continue;
                }
                let terminal_ansi_background = path.file_name().unwrap() == "preview.rs"
                    && line.contains("style = style.bg(Color::Rgb");
                assert!(
                    terminal_ansi_background,
                    "direct chrome background at {}:{}",
                    path.display(),
                    line_number + 1
                );
            }
        }
    }
}
