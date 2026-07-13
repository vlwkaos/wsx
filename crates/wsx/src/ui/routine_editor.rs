use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Clear, Paragraph},
};
use wsx_core::routine::Routine;

#[derive(Clone)]
pub struct RoutineForm {
    pub name: String,
    pub cron: String,
    pub command_json: String,
    pub prompt: String,
    pub field: usize,
    pub cursor: usize,
}

impl RoutineForm {
    pub fn codex() -> Self {
        Self::from_routine(Routine {
            name: String::new(),
            cron: "0 9 * * *".into(),
            command: vec![
                "codex".into(),
                "exec".into(),
                "--json".into(),
                "{prompt}".into(),
            ],
            prompt: String::new(),
        })
    }

    pub fn claude() -> Self {
        Self::from_routine(Routine {
            name: String::new(),
            cron: "0 9 * * *".into(),
            command: vec![
                "claude".into(),
                "-p".into(),
                "--output-format".into(),
                "stream-json".into(),
                "--verbose".into(),
                "{prompt}".into(),
            ],
            prompt: String::new(),
        })
    }

    pub fn from_routine(routine: Routine) -> Self {
        let command_json = serde_json::to_string(&routine.command).unwrap_or_else(|_| "[]".into());
        let cursor = routine.name.len();
        Self {
            name: routine.name,
            cron: routine.cron,
            command_json,
            prompt: routine.prompt,
            field: 0,
            cursor,
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
    pub fn apply_preset(&mut self, claude: bool) {
        let command = if claude {
            Self::claude().command_json
        } else {
            Self::codex().command_json
        };
        self.command_json = command;
        self.field = 2;
        self.cursor = self.command_json.len();
    }
    pub fn routine(&self) -> Result<Routine, String> {
        let command: Vec<String> = serde_json::from_str(&self.command_json)
            .map_err(|e| format!("command must be a JSON argv array: {e}"))?;
        Routine {
            name: self.name.clone(),
            cron: self.cron.clone(),
            command,
            prompt: self.prompt.clone(),
        }
        .validated()
        .map_err(|e| e.to_string())
    }
}

pub fn render(frame: &mut Frame, area: Rect, form: &RoutineForm, editing: bool) {
    let width = area.width.saturating_sub(4).min(88);
    let height = area.height.saturating_sub(2).min(13);
    let popup = super::popup_center(area, width, height);
    frame.render_widget(Clear, popup);
    let labels = ["Name", "Cron", "Command argv (JSON)", "Prompt"];
    let values = [&form.name, &form.cron, &form.command_json, &form.prompt];
    let mut lines = vec![Line::from(if editing {
        "Edit routine"
    } else {
        "Create routine"
    })
    .style(Style::default().fg(Color::Magenta).bold())];
    for (index, (label, value)) in labels.iter().zip(values).enumerate() {
        let marker = if index == form.field { "›" } else { " " };
        lines.push(Line::from(vec![
            Span::styled(
                format!("{marker} {label}: "),
                Style::default().fg(Color::DarkGray),
            ),
            Span::raw(value),
        ]));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "Tab/Shift-Tab field  F1 Codex  F2 Claude  Enter save  Esc cancel",
        Style::default().fg(Color::DarkGray),
    ));
    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" Routine ")),
        popup,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_are_editable_direct_argv_initial_values() {
        let mut form = RoutineForm::codex();
        form.name = "test".into();
        assert_eq!(
            form.routine().unwrap().command,
            vec!["codex", "exec", "--json", "{prompt}"]
        );
        form.apply_preset(true);
        let claude = form.routine().unwrap();
        assert_eq!(claude.command[0], "claude");
        assert!(claude.command.contains(&"stream-json".to_string()));
        form.command_json = "[\"printf\",\"%s\",\"{prompt}\"]".into();
        assert_eq!(form.routine().unwrap().command[0], "printf");
    }

    #[test]
    fn invalid_command_boundary_stays_explicit() {
        let mut form = RoutineForm::codex();
        form.command_json = "codex exec".into();
        assert!(form.routine().unwrap_err().contains("JSON argv array"));
    }

    #[test]
    fn narrow_editor_render_is_safe() {
        let form = RoutineForm::codex();
        let backend = ratatui::backend::TestBackend::new(30, 10);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), &form, false))
            .unwrap();
    }
}
