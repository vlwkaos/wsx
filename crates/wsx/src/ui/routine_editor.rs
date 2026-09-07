use super::{popup_block, theme};
use asched_core::routine::{Routine, Trigger};
use ratatui::{
    prelude::*,
    widgets::{Clear, Paragraph},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoutinePreset {
    Pi,
    Omp,
    Claude,
    Codex,
    Copilot,
    Droid,
    Cursor,
    Kimi,
    Opencode,
    Kilo,
    Hermes,
    Qoder,
    Qwen,
    Antigravity,
    Custom,
}

impl RoutinePreset {
    pub const ALL: [Self; 15] = [
        Self::Pi,
        Self::Omp,
        Self::Claude,
        Self::Codex,
        Self::Copilot,
        Self::Droid,
        Self::Cursor,
        Self::Kimi,
        Self::Opencode,
        Self::Kilo,
        Self::Hermes,
        Self::Qoder,
        Self::Qwen,
        Self::Antigravity,
        Self::Custom,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Pi => "Pi",
            Self::Omp => "OMP",
            Self::Claude => "Claude Code",
            Self::Codex => "Codex",
            Self::Copilot => "GitHub Copilot CLI",
            Self::Droid => "Factory Droid",
            Self::Cursor => "Cursor Agent",
            Self::Kimi => "Kimi Code",
            Self::Opencode => "OpenCode",
            Self::Kilo => "Kilo Code",
            Self::Hermes => "Hermes Agent",
            Self::Qoder => "Qoder CLI",
            Self::Qwen => "Qwen Code",
            Self::Antigravity => "Antigravity CLI",
            Self::Custom => "Custom",
        }
    }

    pub const fn integration_target(self) -> Option<wsx_core::integration::IntegrationTarget> {
        use wsx_core::integration::IntegrationTarget;
        match self {
            Self::Pi => Some(IntegrationTarget::Pi),
            Self::Omp => Some(IntegrationTarget::Omp),
            Self::Claude => Some(IntegrationTarget::Claude),
            Self::Codex => Some(IntegrationTarget::Codex),
            Self::Copilot => Some(IntegrationTarget::Copilot),
            Self::Droid => Some(IntegrationTarget::Droid),
            Self::Cursor => Some(IntegrationTarget::Cursor),
            Self::Kimi => Some(IntegrationTarget::Kimi),
            Self::Opencode => Some(IntegrationTarget::Opencode),
            Self::Kilo => Some(IntegrationTarget::Kilo),
            Self::Hermes => Some(IntegrationTarget::Hermes),
            Self::Qoder => Some(IntegrationTarget::Qodercli),
            Self::Qwen => Some(IntegrationTarget::Qwen),
            Self::Antigravity => Some(IntegrationTarget::AntigravityCli),
            Self::Custom => None,
        }
    }

    pub fn form(self) -> RoutineForm {
        let command = match self {
            Self::Pi => vec!["pi", "-p", "{prompt}"],
            Self::Omp => vec!["omp", "-p", "{prompt}"],
            Self::Claude => vec!["claude", "-p", "{prompt}"],
            Self::Codex => vec!["codex", "exec", "{prompt}"],
            Self::Copilot => vec!["copilot", "-p", "{prompt}"],
            Self::Droid => vec!["droid", "exec", "{prompt}"],
            Self::Cursor => vec!["agent", "-p", "{prompt}"],
            Self::Kimi => vec!["kimi", "-p", "{prompt}"],
            Self::Opencode => vec!["opencode", "run", "{prompt}"],
            Self::Kilo => vec!["kilo", "run", "--auto", "{prompt}"],
            Self::Hermes => vec!["hermes", "-z", "{prompt}"],
            Self::Qoder => vec!["qoder", "-p", "{prompt}"],
            Self::Qwen => vec!["qwen", "-p", "{prompt}"],
            Self::Antigravity => vec!["agy", "-p", "{prompt}"],
            Self::Custom => Vec::new(),
        }
        .into_iter()
        .map(str::to_string)
        .collect();
        let mut form = RoutineForm::from_routine(Routine {
            name: String::new(),
            trigger: Trigger::Cron("0 9 * * *".into()),
            command,
            prompt: String::new(),
            enabled: true,
        });
        form.integration_target = self.integration_target();
        form
    }
}

#[derive(Clone)]
pub struct RoutineForm {
    pub name: String,
    pub cron: String,
    pub command_json: String,
    pub prompt: String,
    pub field: usize,
    pub cursor: usize,
    pub integration_target: Option<wsx_core::integration::IntegrationTarget>,
    enabled: bool,
}

impl RoutineForm {
    pub fn from_routine(routine: Routine) -> Self {
        let command_json = serde_json::to_string(&routine.command).unwrap_or_else(|_| "[]".into());
        let cursor = routine.name.len();
        Self {
            name: routine.name,
            cron: match routine.trigger {
                Trigger::Cron(cron) => cron,
                Trigger::Event { kind } => format!("event:{kind}"),
            },
            command_json,
            prompt: routine.prompt,
            field: 0,
            cursor,
            integration_target: None,
            enabled: routine.enabled,
        }
    }

    fn value_mut(&mut self) -> &mut String {
        match self.field {
            0 => &mut self.name,
            1 => &mut self.cron,
            2 => &mut self.command_json,
            _ => &mut self.prompt,
        }
    }

    pub fn insert(&mut self, c: char) {
        let cursor = self.cursor.min(self.value_mut().len());
        self.value_mut().insert(cursor, c);
        self.cursor = cursor + c.len_utf8();
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let cursor = self.cursor;
        let value = self.value_mut();
        let previous = value[..cursor]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0);
        value.drain(previous..cursor);
        self.cursor = previous;
    }

    pub fn left(&mut self) {
        let cursor = self.cursor;
        self.cursor = self.value_mut()[..cursor]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0);
    }
    pub fn right(&mut self) {
        let cursor = self.cursor;
        let value = self.value_mut();
        let advance = if cursor < value.len() {
            value[cursor..]
                .chars()
                .next()
                .map(char::len_utf8)
                .unwrap_or(0)
        } else {
            0
        };
        self.cursor = cursor + advance;
    }
    pub fn next(&mut self, backwards: bool) {
        self.field = if backwards {
            (self.field + 3) % 4
        } else {
            (self.field + 1) % 4
        };
        self.cursor = self.value_mut().len();
    }
    pub fn routine(&self) -> Result<Routine, String> {
        let command: Vec<String> = serde_json::from_str(&self.command_json)
            .map_err(|e| format!("command must be a JSON argv array: {e}"))?;
        let trigger = self
            .cron
            .strip_prefix("event:")
            .map(|kind| Trigger::Event {
                kind: kind.to_string(),
            })
            .unwrap_or_else(|| Trigger::Cron(self.cron.clone()));
        Routine {
            name: self.name.clone(),
            trigger,
            command,
            prompt: self.prompt.clone(),
            enabled: self.enabled,
        }
        .validated()
        .map_err(|e| e.to_string())
    }
}

pub fn render_preset_picker(frame: &mut Frame, area: Rect, selected: usize) {
    let width = area.width.saturating_sub(4).min(52);
    let height = area.height.saturating_sub(2).min(19);
    let popup = super::popup_center(area, width, height);
    frame.render_widget(Clear, popup);
    let visible = usize::from(popup.height.saturating_sub(2)).max(1);
    let start = selected
        .saturating_sub(visible.saturating_sub(1))
        .min(RoutinePreset::ALL.len().saturating_sub(visible));
    let lines = RoutinePreset::ALL
        .iter()
        .enumerate()
        .skip(start)
        .take(visible)
        .map(|(index, preset)| {
            let marker = if index == selected { "›" } else { " " };
            let style = if index == selected {
                Style::default().fg(theme::ACCENT).bold()
            } else {
                Style::default().fg(theme::TEXT)
            };
            Line::from(format!("{marker} {}", preset.label())).style(style)
        })
        .collect::<Vec<_>>();
    let block = popup_block(
        Line::from(" Create routine "),
        Line::from(" (j/k)select  (Enter)continue  (Esc)cancel "),
        Style::default().fg(theme::ACCENT),
    );
    frame.render_widget(Paragraph::new(lines).block(block), popup);
}

pub fn render(frame: &mut Frame, area: Rect, form: &RoutineForm, editing: bool, can_rename: bool) {
    let width = area.width.saturating_sub(4).min(88);
    let height = area.height.saturating_sub(2).min(13);
    let popup = super::popup_center(area, width, height);
    frame.render_widget(Clear, popup);
    let labels = ["Name", "Trigger", "Command argv (JSON)", "Prompt"];
    let values = [&form.name, &form.cron, &form.command_json, &form.prompt];
    let mut cursor_position = None;
    let mut lines = vec![Line::from(if editing {
        "Edit routine"
    } else {
        "Create routine"
    })
    .style(Style::default().fg(theme::ACCENT).bold())];
    for (index, (label, value)) in labels.iter().zip(values).enumerate() {
        let marker = if index == form.field { "›" } else { " " };
        let locked = index == 0 && editing && !can_rename;
        let label = if locked {
            format!("{label} (locked)")
        } else {
            (*label).to_string()
        };
        let value_width = popup
            .width
            .saturating_sub(label.len() as u16)
            .saturating_sub(7) as usize;
        let (value, cursor_column) = visible_value(
            value,
            (index == form.field).then_some(form.cursor),
            value_width,
        );
        let prefix = format!("{marker} {label}: ");
        if let Some(cursor_column) = cursor_column {
            cursor_position = Some(Position::new(
                popup.x + 1 + Span::raw(prefix.as_str()).width() as u16 + cursor_column as u16,
                popup.y + 2 + index as u16,
            ));
        }
        lines.push(Line::from(vec![
            Span::styled(prefix, Style::default().fg(theme::TEXT_SUBTLE)),
            Span::raw(value),
        ]));
    }
    let hints = Line::from(vec![
        Span::styled(
            " Tab/Shift-Tab field",
            Style::default().fg(theme::TEXT_SUBTLE),
        ),
        Span::styled("  Enter save", Style::default().fg(theme::TEXT)),
        Span::styled("  Esc cancel ", Style::default().fg(theme::TEXT_MUTED)),
    ]);
    let block = popup_block(
        Line::from(" Routine "),
        hints,
        Style::default().fg(theme::ACCENT),
    );
    frame.render_widget(Paragraph::new(lines).block(block), popup);
    if let Some(position) = cursor_position {
        frame.set_cursor_position(position);
    }
}

fn visible_value(value: &str, cursor: Option<usize>, width: usize) -> (String, Option<usize>) {
    if width == 0 {
        return (String::new(), cursor.map(|_| 0));
    }
    let chars = value.chars().collect::<Vec<_>>();
    let widths = chars
        .iter()
        .map(|character| Span::raw(character.to_string()).width())
        .collect::<Vec<_>>();
    let mut cumulative_widths = Vec::with_capacity(widths.len() + 1);
    cumulative_widths.push(0);
    for character_width in &widths {
        cumulative_widths.push(cumulative_widths.last().copied().unwrap_or(0) + character_width);
    }
    let cursor_chars = cursor
        .map(|cursor| value[..cursor.min(value.len())].chars().count())
        .unwrap_or(0);
    if cumulative_widths.last().copied().unwrap_or(0) <= width {
        let cursor_column = cursor.map(|_| cumulative_widths[cursor_chars]);
        return (value.to_string(), cursor_column);
    }
    let mut start = 0;
    if cursor.is_some() {
        while start < cursor_chars
            && cumulative_widths[cursor_chars] - cumulative_widths[start] + usize::from(start > 0)
                > width
        {
            start += 1;
        }
    }
    // ^ A leading ellipsis consumes one viewport column; tail tests must count it.
    let prefix = usize::from(start > 0);
    let mut visible = String::new();
    if prefix == 1 {
        visible.push('…');
    }
    let mut visible_width = prefix;
    let required_end = cursor.map(|_| cursor_chars).unwrap_or(start);
    let mut index = start;
    while index < required_end {
        visible.push(chars[index]);
        visible_width += widths[index];
        index += 1;
    }
    let cursor_column = cursor.map(|_| visible_width);
    while index < chars.len() {
        let suffix_width = usize::from(index + 1 < chars.len());
        if visible_width + widths[index] + suffix_width > width {
            if visible_width < width {
                visible.push('…');
            }
            break;
        }
        visible.push(chars[index]);
        visible_width += widths[index];
        index += 1;
    }
    (visible, cursor_column)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_routine_form_builds_an_enabled_routine() {
        let mut form = RoutinePreset::Codex.form();
        form.name = "daily".into();

        assert!(form.routine().unwrap().enabled);
    }

    #[test]
    fn disabled_routine_form_round_trip_preserves_disabled_state() {
        let mut form = RoutineForm::from_routine(Routine {
            name: "daily".into(),
            trigger: Trigger::Cron("0 9 * * *".into()),
            command: vec!["echo".into()],
            prompt: "before".into(),
            enabled: false,
        });
        form.prompt = "after".into();

        let saved = form.routine().unwrap();
        assert_eq!(saved.prompt, "after");
        assert!(!saved.enabled);
    }

    #[test]
    fn event_trigger_round_trip_preserves_provider_neutral_kind() {
        let form = RoutineForm::from_routine(Routine {
            name: "changed".into(),
            trigger: Trigger::Event {
                kind: "filesystem.changed".into(),
            },
            command: vec!["echo".into()],
            prompt: String::new(),
            enabled: true,
        });

        assert_eq!(
            form.routine().unwrap().trigger,
            Trigger::Event {
                kind: "filesystem.changed".into()
            }
        );
    }

    #[test]
    fn presets_are_editable_direct_argv_initial_values() {
        let mut codex = RoutinePreset::Codex.form();
        codex.name = "test".into();
        assert_eq!(
            codex.routine().unwrap().command,
            vec!["codex", "exec", "{prompt}"]
        );
        let mut claude = RoutinePreset::Claude.form();
        claude.name = "test".into();
        assert_eq!(
            claude.routine().unwrap().command,
            vec!["claude", "-p", "{prompt}"]
        );
        codex.command_json = "[\"printf\",\"%s\",\"{prompt}\"]".into();
        assert_eq!(codex.routine().unwrap().command[0], "printf");
    }

    #[test]
    fn documented_presets_and_custom_keep_command_validation_explicit() {
        let mut pi = RoutinePreset::Pi.form();
        pi.name = "review".into();
        assert_eq!(pi.routine().unwrap().command, vec!["pi", "-p", "{prompt}"]);

        let commands = [
            (RoutinePreset::Omp, "omp"),
            (RoutinePreset::Droid, "droid"),
            (RoutinePreset::Cursor, "agent"),
            (RoutinePreset::Kimi, "kimi"),
            (RoutinePreset::Opencode, "opencode"),
            (RoutinePreset::Kilo, "kilo"),
            (RoutinePreset::Hermes, "hermes"),
            (RoutinePreset::Qoder, "qoder"),
            (RoutinePreset::Qwen, "qwen"),
            (RoutinePreset::Antigravity, "agy"),
        ];
        for (preset, executable) in commands {
            let mut form = preset.form();
            form.name = "test".into();
            assert_eq!(form.routine().unwrap().command[0], executable);
            assert_eq!(preset.integration_target().unwrap().label(), preset.label());
        }

        let mut custom = RoutinePreset::Custom.form();
        custom.name = "custom".into();
        assert!(custom
            .routine()
            .unwrap_err()
            .contains("command must contain nonempty argv items"));
        custom.command_json = "[\"printf\",\"%s\",\"{prompt}\"]".into();
        assert_eq!(custom.routine().unwrap().command[0], "printf");
    }

    #[test]
    fn preset_picker_renders_named_choices_and_contextual_hints() {
        let backend = ratatui::backend::TestBackend::new(60, 22);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_preset_picker(frame, frame.area(), 2))
            .unwrap();
        let text =
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .fold(String::new(), |mut text, cell| {
                    text.push_str(cell.symbol());
                    text
                });
        for expected in ["Pi", "Claude Code", "Codex", "Custom", "(j/k)select"] {
            assert!(text.contains(expected), "missing {expected:?} in {text:?}");
        }
    }

    #[test]
    fn invalid_command_boundary_stays_explicit() {
        let mut form = RoutinePreset::Codex.form();
        form.command_json = "codex exec".into();
        assert!(form.routine().unwrap_err().contains("JSON argv array"));
    }

    #[test]
    fn narrow_editor_and_picker_render_safely() {
        let form = RoutinePreset::Codex.form();
        let backend = ratatui::backend::TestBackend::new(30, 10);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), &form, false, true))
            .unwrap();
        terminal
            .draw(|frame| render_preset_picker(frame, frame.area(), 0))
            .unwrap();
    }

    #[test]
    fn routine_actions_render_on_the_bottom_border() {
        let form = RoutinePreset::Codex.form();
        let backend = ratatui::backend::TestBackend::new(88, 13);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| render(frame, frame.area(), &form, false, true))
            .unwrap();

        let rows = (0..13)
            .map(|y| {
                (0..88)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        let bottom_y = rows
            .iter()
            .position(|row| row.contains("Enter save"))
            .expect("routine hints");
        let bottom = &rows[bottom_y];
        assert!(bottom.contains("Esc cancel"), "{bottom:?}");
        assert_eq!(
            terminal.backend().buffer()[(2, bottom_y as u16)].symbol(),
            "└"
        );
        assert_eq!(
            terminal.backend().buffer()[(85, bottom_y as u16)].symbol(),
            "┘"
        );
        for (y, row) in rows.iter().enumerate() {
            if y != bottom_y {
                assert!(!row.contains("Enter save"), "{row:?}");
                assert!(!row.contains("Esc cancel"), "{row:?}");
            }
        }
    }

    #[test]
    fn active_long_field_scrolls_to_its_cursor() {
        let value = "head-abcdefghijklmnopqrstuvwxyz-tail";
        assert_eq!(
            visible_value(value, Some(value.len()), 12),
            ("…uvwxyz-tail".into(), Some(12))
        );
        assert_eq!(
            visible_value(value, None, 12),
            ("head-abcdef…".into(), None)
        );
    }

    #[test]
    fn editor_places_terminal_cursor_in_active_scrolled_field() {
        let mut form = RoutinePreset::Codex.form();
        form.command_json = "[\"head-abcdefghijklmnopqrstuvwxyz-tail\"]".into();
        form.field = 2;
        form.cursor = form.command_json.len();
        let backend = ratatui::backend::TestBackend::new(40, 12);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| render(frame, frame.area(), &form, false, true))
            .unwrap();

        terminal
            .backend_mut()
            .assert_cursor_position(Position::new(36, 5));
    }

    #[test]
    fn wide_characters_use_terminal_columns_for_cursor_and_viewport() {
        assert_eq!(
            visible_value("가나다라마바사", Some("가나다라마바사".len()), 7),
            ("…마바사".into(), Some(7))
        );
    }
}
