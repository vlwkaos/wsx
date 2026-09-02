use crate::action::Action;
use anyhow::Result;
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
struct KeyChord {
    code: KeyCode,
    modifiers: KeyModifiers,
    label: String,
}

impl KeyChord {
    fn parse(value: &str, modifier_required: bool) -> Option<Self> {
        let parts = value
            .split('+')
            .map(|part| part.trim().to_ascii_lowercase())
            .collect::<Vec<_>>();
        let (key, modifiers) = parts.split_last()?;
        let mut flags = KeyModifiers::empty();
        for modifier in modifiers {
            flags |= match modifier.as_str() {
                "ctrl" | "control" => KeyModifiers::CONTROL,
                "alt" => KeyModifiers::ALT,
                "shift" => KeyModifiers::SHIFT,
                "super" | "cmd" => KeyModifiers::SUPER,
                _ => return None,
            };
        }
        if modifier_required && flags.is_empty() {
            return None;
        }
        let code = match key.as_str() {
            "space" => KeyCode::Char(' '),
            "escape" | "esc" => KeyCode::Esc,
            "tab" => KeyCode::Tab,
            value if value.chars().count() == 1 => KeyCode::Char(value.chars().next()?),
            _ => return None,
        };
        let label = parts
            .iter()
            .map(|part| match part.as_str() {
                "ctrl" | "control" => "Ctrl".into(),
                "alt" => "Alt".into(),
                "shift" => "Shift".into(),
                "super" | "cmd" => "Super".into(),
                "space" => "Space".into(),
                "escape" | "esc" => "Esc".into(),
                "tab" => "Tab".into(),
                other => other.to_uppercase(),
            })
            .collect::<Vec<String>>()
            .join("+");
        Some(Self {
            code,
            modifiers: flags,
            label,
        })
    }

    fn matches(&self, key: KeyEvent) -> bool {
        key.code == self.code && key.modifiers == self.modifiers
    }

    fn matches_after_prefix(&self, key: KeyEvent, prefix: &Self) -> bool {
        self.code == key.code
            && (key.modifiers == self.modifiers
                || key.modifiers == self.modifiers | prefix.modifiers)
    }

    fn key_event(&self) -> KeyEvent {
        KeyEvent::new(self.code, self.modifiers)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EventMode {
    Normal,
    Input,
    Workspace,
    Terminal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EscapeSequence {
    prefix: KeyChord,
    suffix: Option<KeyChord>,
    pending_prefix: bool,
    pub label: String,
}

#[derive(Debug, PartialEq, Eq)]
enum TerminalEscapeAction {
    Escape,
    Quit,
    Forward(Vec<KeyEvent>),
    Pending,
}

#[derive(Debug, PartialEq, Eq)]
enum WorkspaceEscapeAction {
    Quit,
    Forward(KeyEvent),
    Pending,
}

impl EscapeSequence {
    pub fn parse(value: &str) -> Option<Self> {
        let parts = value.split_whitespace().collect::<Vec<_>>();
        if parts.is_empty() || parts.len() > 2 {
            return None;
        }
        let prefix = KeyChord::parse(parts[0], true)?;
        let suffix = match parts.get(1) {
            Some(part) => {
                let suffix = KeyChord::parse(part, false)?;
                if suffix.code == KeyCode::Char('q') {
                    return None;
                }
                Some(suffix)
            }
            None => None,
        };
        let label = suffix.as_ref().map_or_else(
            || prefix.label.clone(),
            |suffix| format!("{} {}", prefix.label, suffix.label),
        );
        Some(Self {
            prefix,
            suffix,
            pending_prefix: false,
            label,
        })
    }

    fn terminal_key(&mut self, key: KeyEvent) -> TerminalEscapeAction {
        if key.kind == KeyEventKind::Release {
            return TerminalEscapeAction::Pending;
        }
        let Some(suffix) = &self.suffix else {
            return if self.prefix.matches(key) {
                TerminalEscapeAction::Escape
            } else {
                TerminalEscapeAction::Forward(vec![key])
            };
        };
        if self.pending_prefix {
            self.pending_prefix = false;
            if self.matches_prefixed_quit(key) {
                TerminalEscapeAction::Quit
            } else if suffix.matches_after_prefix(key, &self.prefix) {
                TerminalEscapeAction::Escape
            } else if self.prefix.matches(key) {
                TerminalEscapeAction::Forward(vec![self.prefix.key_event()])
            } else {
                TerminalEscapeAction::Forward(vec![self.prefix.key_event(), key])
            }
        } else if self.prefix.matches(key) {
            self.pending_prefix = true;
            TerminalEscapeAction::Pending
        } else {
            TerminalEscapeAction::Forward(vec![key])
        }
    }

    fn workspace_key(&mut self, key: KeyEvent) -> WorkspaceEscapeAction {
        if key.kind == KeyEventKind::Release {
            return WorkspaceEscapeAction::Forward(key);
        }
        let Some(suffix) = &self.suffix else {
            return WorkspaceEscapeAction::Forward(key);
        };
        if self.pending_prefix {
            self.pending_prefix = false;
            if self.matches_prefixed_quit(key) {
                WorkspaceEscapeAction::Quit
            } else if suffix.matches_after_prefix(key, &self.prefix) {
                WorkspaceEscapeAction::Pending
            } else {
                WorkspaceEscapeAction::Forward(key)
            }
        } else if self.prefix.matches(key) {
            self.pending_prefix = true;
            WorkspaceEscapeAction::Pending
        } else {
            WorkspaceEscapeAction::Forward(key)
        }
    }

    fn matches_prefixed_quit(&self, key: KeyEvent) -> bool {
        key.code == KeyCode::Char('q')
            && (key.modifiers.is_empty() || key.modifiers == self.prefix.modifiers)
    }

    fn take_pending_prefix(&mut self) -> Option<KeyEvent> {
        if self.pending_prefix {
            self.pending_prefix = false;
            Some(self.prefix.key_event())
        } else {
            None
        }
    }

    fn matches_single(&self, key: KeyEvent) -> bool {
        self.suffix.is_none() && self.prefix.matches(key)
    }

    pub fn literal_key_event(&self) -> KeyEvent {
        self.prefix.key_event()
    }

    pub fn literal_label(&self) -> String {
        if self.suffix.is_some() {
            format!("{} {}", self.prefix.label, self.prefix.label)
        } else {
            format!("{} in Workspace", self.prefix.label)
        }
    }

    pub fn quit_label(&self) -> Option<String> {
        self.suffix
            .as_ref()
            .map(|_| format!("{} Q", self.prefix.label))
    }

    pub fn reset(&mut self) {
        self.pending_prefix = false;
    }
}

pub fn poll_event(
    timeout: Duration,
    mode: EventMode,
    escape: &mut EscapeSequence,
) -> Result<Option<Action>> {
    if event::poll(timeout)? {
        let action = match event::read()? {
            Event::Key(key) if mode == EventMode::Terminal => match escape.terminal_key(key) {
                TerminalEscapeAction::Escape => Action::InputEscape,
                TerminalEscapeAction::Quit => Action::Quit,
                TerminalEscapeAction::Forward(keys) if keys.len() == 1 => {
                    Action::TerminalKey(keys[0])
                }
                TerminalEscapeAction::Forward(keys) => Action::TerminalKeys(keys),
                TerminalEscapeAction::Pending => Action::None,
            },
            Event::Key(key) if mode == EventMode::Workspace && escape.matches_single(key) => {
                Action::LiteralEscape
            }
            Event::Key(key) if mode == EventMode::Workspace => match escape.workspace_key(key) {
                WorkspaceEscapeAction::Quit => Action::Quit,
                WorkspaceEscapeAction::Forward(key) => translate_key(key),
                WorkspaceEscapeAction::Pending => Action::None,
            },
            Event::Key(key) => {
                if mode == EventMode::Input {
                    translate_input_key(key)
                } else {
                    translate_key(key)
                }
            }
            Event::Mouse(mouse) if mode == EventMode::Terminal => escape
                .take_pending_prefix()
                .map_or(Action::TerminalMouse(mouse), |prefix| {
                    Action::TerminalPrefixedMouse(prefix, mouse)
                }),
            Event::Mouse(mouse) if mode == EventMode::Workspace => {
                escape.reset();
                translate_mouse(mouse)
            }
            Event::Mouse(mouse) => translate_mouse(mouse),
            Event::Paste(text) if mode == EventMode::Terminal => match escape.take_pending_prefix()
            {
                Some(prefix) => Action::TerminalPrefixedPaste(prefix, text),
                None => Action::TerminalPaste(text),
            },
            Event::Paste(_) if mode == EventMode::Workspace => {
                escape.reset();
                Action::None
            }
            Event::Paste(_) => Action::None,
            Event::Resize(_, _) if mode == EventMode::Workspace => {
                escape.reset();
                Action::Resize
            }
            Event::Resize(_, _) => Action::Resize,
            _ => Action::None,
        };
        Ok(Some(action))
    } else {
        Ok(None)
    }
}

/// Input mode: only special keys are translated; all chars go to the buffer.
fn translate_input_key(key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Enter => Action::Select,
        KeyCode::Esc => Action::InputEscape,
        KeyCode::Backspace => Action::InputBackspace,
        KeyCode::Tab => Action::InputTab,
        KeyCode::BackTab => Action::InputBackTab,
        KeyCode::Down => Action::NavigateDown,
        KeyCode::Up => Action::NavigateUp,
        KeyCode::Left => Action::NavigateLeft,
        KeyCode::Right => Action::NavigateRight,
        KeyCode::Char(c) => Action::InputChar(c),
        _ => Action::None,
    }
}

fn translate_mouse(mouse: MouseEvent) -> Action {
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => Action::MouseClick {
            col: mouse.column,
            row: mouse.row,
        },
        MouseEventKind::ScrollUp | MouseEventKind::ScrollLeft => Action::MouseScroll {
            col: mouse.column,
            row: mouse.row,
            delta: -1,
        },
        MouseEventKind::ScrollDown | MouseEventKind::ScrollRight => Action::MouseScroll {
            col: mouse.column,
            row: mouse.row,
            delta: 1,
        },
        _ => Action::None,
    }
}

// ^ [[Keybindings]] Scope-specific meaning stays in dispatch, not translation.
fn translate_key(key: KeyEvent) -> Action {
    match (key.modifiers, key.code) {
        (KeyModifiers::NONE, KeyCode::Char('q')) => Action::Quit,
        (KeyModifiers::SHIFT, KeyCode::Char('Q')) | (KeyModifiers::NONE, KeyCode::Char('Q')) => {
            Action::HardQuit
        }
        (KeyModifiers::NONE, KeyCode::Char('j')) | (KeyModifiers::NONE, KeyCode::Down) => {
            Action::NavigateDown
        }
        (KeyModifiers::NONE, KeyCode::Char('k')) | (KeyModifiers::NONE, KeyCode::Up) => {
            Action::NavigateUp
        }
        (KeyModifiers::NONE, KeyCode::Char('h')) | (KeyModifiers::NONE, KeyCode::Left) => {
            Action::NavigateLeft
        }
        (KeyModifiers::NONE, KeyCode::Char('l')) | (KeyModifiers::NONE, KeyCode::Right) => {
            Action::NavigateRight
        }
        (KeyModifiers::NONE, KeyCode::Enter) => Action::Select,
        (KeyModifiers::NONE, KeyCode::Char('p')) => Action::AddProject,
        (KeyModifiers::NONE, KeyCode::Char('w')) => Action::AddWorktree,
        (KeyModifiers::NONE, KeyCode::Char('s')) => Action::AddSession,
        (KeyModifiers::SHIFT, KeyCode::Char('|')) | (KeyModifiers::NONE, KeyCode::Char('|')) => {
            Action::SplitPaneVertical
        }
        (KeyModifiers::NONE, KeyCode::Char('-')) => Action::SplitPaneHorizontal,
        (KeyModifiers::NONE, KeyCode::Char('u')) => Action::AddRoutine,
        (KeyModifiers::NONE, KeyCode::Char('d')) => Action::Delete,
        (KeyModifiers::NONE, KeyCode::Char('c')) => Action::Clean,
        (KeyModifiers::NONE, KeyCode::Char('e')) => Action::Edit,
        (KeyModifiers::NONE, KeyCode::Char(',')) => Action::EditGlobalConfig,
        (KeyModifiers::NONE, KeyCode::Char('r')) => Action::SetAlias,
        (KeyModifiers::SHIFT, KeyCode::Char('R')) | (KeyModifiers::NONE, KeyCode::Char('R')) => {
            Action::Refresh
        }
        (KeyModifiers::NONE, KeyCode::Char('?')) => Action::Help,
        (KeyModifiers::NONE, KeyCode::Char('y')) => Action::ConfirmYes,
        (KeyModifiers::NONE, KeyCode::Char('n')) => Action::NextAttention,
        (KeyModifiers::SHIFT, KeyCode::Char('N')) | (KeyModifiers::NONE, KeyCode::Char('N')) => {
            Action::PrevAttention
        }
        (KeyModifiers::NONE, KeyCode::Char('x')) => Action::DismissAttention,
        (KeyModifiers::NONE, KeyCode::Char('m')) => Action::EnterMove,
        (KeyModifiers::NONE, KeyCode::Char(']')) => Action::JumpProjectDown,
        (KeyModifiers::NONE, KeyCode::Char('[')) => Action::JumpProjectUp,
        (KeyModifiers::NONE, KeyCode::Char('a')) => Action::NextActive,
        (KeyModifiers::SHIFT, KeyCode::Char('A')) | (KeyModifiers::NONE, KeyCode::Char('A')) => {
            Action::PrevActive
        }
        (KeyModifiers::NONE, KeyCode::Char('i')) => Action::NextIdle,
        (KeyModifiers::SHIFT, KeyCode::Char('I')) | (KeyModifiers::NONE, KeyCode::Char('I')) => {
            Action::PrevIdle
        }
        (KeyModifiers::SHIFT, KeyCode::Char('C')) | (KeyModifiers::NONE, KeyCode::Char('C')) => {
            Action::SendCtrlC
        }
        (KeyModifiers::NONE, KeyCode::Char('g')) => Action::AssignGroup,
        (KeyModifiers::NONE, KeyCode::Char('/')) => Action::SearchStart,
        (KeyModifiers::SHIFT, KeyCode::Char('{')) | (KeyModifiers::NONE, KeyCode::Char('{')) => {
            Action::GroupPrev
        }
        (KeyModifiers::SHIFT, KeyCode::Char('}')) | (KeyModifiers::NONE, KeyCode::Char('}')) => {
            Action::GroupNext
        }
        (KeyModifiers::SHIFT, KeyCode::Char('T')) | (KeyModifiers::NONE, KeyCode::Char('T')) => {
            Action::GroupManager
        }
        (KeyModifiers::NONE, KeyCode::Esc) => Action::InputEscape,
        (KeyModifiers::NONE, KeyCode::Backspace) => Action::InputBackspace,
        _ => Action::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_sequence_requires_a_modified_prefix_and_normalizes_its_label() {
        assert!(EscapeSequence::parse("space w").is_none());
        let sequence = EscapeSequence::parse("control+a w").unwrap();
        assert_eq!(sequence.label, "Ctrl+A W");
    }

    #[test]
    fn escape_sequence_reserves_q_for_prefixed_quit() {
        assert!(EscapeSequence::parse("ctrl+a q").is_none());
        assert!(EscapeSequence::parse("ctrl+a shift+q").is_none());
        assert_eq!(
            EscapeSequence::parse("ctrl+a w").unwrap().quit_label(),
            Some("Ctrl+A Q".into())
        );
    }

    #[test]
    fn escape_sequence_forwards_unknown_suffix_and_doubles_literal_prefix() {
        let prefix = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL);
        let x = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
        let mut sequence = EscapeSequence::parse("ctrl+a w").unwrap();
        assert_eq!(sequence.terminal_key(prefix), TerminalEscapeAction::Pending);
        assert_eq!(
            sequence.terminal_key(x),
            TerminalEscapeAction::Forward(vec![prefix, x])
        );
        assert_eq!(sequence.terminal_key(prefix), TerminalEscapeAction::Pending);
        assert_eq!(
            sequence.terminal_key(prefix),
            TerminalEscapeAction::Forward(vec![prefix])
        );
    }

    #[test]
    fn escape_sequence_flushes_pending_prefix_before_non_key_input() {
        let prefix = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL);
        let mut sequence = EscapeSequence::parse("ctrl+a w").unwrap();
        assert_eq!(sequence.terminal_key(prefix), TerminalEscapeAction::Pending);
        assert_eq!(sequence.take_pending_prefix(), Some(prefix));
        assert_eq!(sequence.take_pending_prefix(), None);
    }

    #[test]
    fn escape_sequence_focuses_workspace_on_suffix() {
        let mut sequence = EscapeSequence::parse("ctrl+a w").unwrap();
        assert_eq!(
            sequence.terminal_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL)),
            TerminalEscapeAction::Pending
        );
        assert_eq!(
            sequence.terminal_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE)),
            TerminalEscapeAction::Escape
        );
    }

    #[test]
    fn escape_sequence_quits_on_prefixed_q_with_or_without_held_modifier() {
        let prefix = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL);
        let mut sequence = EscapeSequence::parse("ctrl+a w").unwrap();

        for modifiers in [KeyModifiers::NONE, KeyModifiers::CONTROL] {
            assert_eq!(sequence.terminal_key(prefix), TerminalEscapeAction::Pending);
            assert_eq!(
                sequence.terminal_key(KeyEvent::new(KeyCode::Char('q'), modifiers)),
                TerminalEscapeAction::Quit
            );
        }
    }

    #[test]
    fn workspace_sequence_quits_consumes_focus_and_forwards_unknown_suffixes() {
        let prefix = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL);
        let mut sequence = EscapeSequence::parse("ctrl+a w").unwrap();

        for modifiers in [KeyModifiers::NONE, KeyModifiers::CONTROL] {
            assert_eq!(
                sequence.workspace_key(prefix),
                WorkspaceEscapeAction::Pending
            );
            assert_eq!(
                sequence.workspace_key(KeyEvent::new(KeyCode::Char('q'), modifiers)),
                WorkspaceEscapeAction::Quit
            );
        }
        assert_eq!(
            sequence.workspace_key(prefix),
            WorkspaceEscapeAction::Pending
        );
        assert_eq!(
            sequence.workspace_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE)),
            WorkspaceEscapeAction::Pending
        );
        assert_eq!(
            sequence.workspace_key(prefix),
            WorkspaceEscapeAction::Pending
        );
        let edit = KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE);
        assert_eq!(
            sequence.workspace_key(edit),
            WorkspaceEscapeAction::Forward(edit)
        );
        assert_eq!(translate_key(edit), Action::Edit);

        assert_eq!(
            sequence.workspace_key(prefix),
            WorkspaceEscapeAction::Pending
        );
        sequence.reset();
        let quit = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        assert_eq!(
            sequence.workspace_key(quit),
            WorkspaceEscapeAction::Forward(quit)
        );
    }

    #[test]
    fn single_chord_focus_behavior_has_no_prefixed_quit() {
        let chord = KeyEvent::new(KeyCode::Char('g'), KeyModifiers::ALT);
        let mut sequence = EscapeSequence::parse("alt+g").unwrap();

        assert_eq!(sequence.quit_label(), None);
        assert_eq!(sequence.terminal_key(chord), TerminalEscapeAction::Escape);
        assert_eq!(
            sequence.workspace_key(chord),
            WorkspaceEscapeAction::Forward(chord)
        );
    }

    #[test]
    fn escape_sequence_accepts_suffix_while_prefix_modifier_is_still_held() {
        let mut sequence = EscapeSequence::parse("ctrl+a w").unwrap();
        assert_eq!(
            sequence.terminal_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL)),
            TerminalEscapeAction::Pending
        );
        assert_eq!(
            sequence.terminal_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL)),
            TerminalEscapeAction::Escape
        );
    }

    #[test]
    fn uppercase_q_is_a_distinct_hard_quit_action() {
        assert_eq!(
            translate_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
            Action::Quit
        );
        let uppercase_q = KeyEvent::new(KeyCode::Char('Q'), KeyModifiers::SHIFT);
        assert_eq!(translate_key(uppercase_q), Action::HardQuit);
        assert_eq!(translate_input_key(uppercase_q), Action::InputChar('Q'));
        assert_eq!(
            translate_input_key(KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE)),
            Action::None
        );
        assert_eq!(
            translate_input_key(KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE)),
            Action::None
        );

        let mut terminal_escape = EscapeSequence::parse("ctrl+a w").unwrap();
        assert_eq!(
            terminal_escape.terminal_key(uppercase_q),
            TerminalEscapeAction::Forward(vec![uppercase_q])
        );
    }

    #[test]
    fn comma_opens_global_config_only_from_workspace_translation() {
        let comma = KeyEvent::new(KeyCode::Char(','), KeyModifiers::NONE);
        assert_eq!(translate_key(comma), Action::EditGlobalConfig);

        let mut terminal_escape = EscapeSequence::parse("ctrl+a w").unwrap();
        assert_eq!(
            terminal_escape.terminal_key(comma),
            TerminalEscapeAction::Forward(vec![comma])
        );
    }

    #[test]
    fn workspace_removes_send_text_but_keeps_interrupt_and_group_assignment() {
        let send = KeyEvent::new(KeyCode::Char('S'), KeyModifiers::SHIFT);
        let interrupt = KeyEvent::new(KeyCode::Char('C'), KeyModifiers::SHIFT);
        let assign = KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE);
        assert_eq!(translate_key(send), Action::None);
        assert_eq!(translate_key(interrupt), Action::SendCtrlC);
        assert_eq!(translate_key(assign), Action::AssignGroup);
    }

    #[test]
    fn workspace_mouse_wheel_preserves_header_coordinates_and_direction() {
        let up = translate_mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 12,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(
            up,
            Action::MouseScroll {
                col: 12,
                row: 0,
                delta: -1,
            }
        );
    }

    #[test]
    fn pane_split_keys_are_direct_workspace_actions() {
        assert_eq!(
            translate_key(KeyEvent::new(KeyCode::Char('|'), KeyModifiers::SHIFT)),
            Action::SplitPaneVertical
        );
        assert_eq!(
            translate_key(KeyEvent::new(KeyCode::Char('-'), KeyModifiers::NONE)),
            Action::SplitPaneHorizontal
        );
    }
}
