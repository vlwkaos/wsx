use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use std::time::Duration;
use anyhow::Result;
use crate::action::Action;

pub fn poll_event(timeout: Duration, in_input: bool) -> Result<Option<Action>> {
    if event::poll(timeout)? {
        let action = match event::read()? {
            Event::Key(key) => {
                if in_input { translate_input_key(key) } else { translate_key(key) }
            }
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
        KeyCode::Down => Action::NavigateDown,
        KeyCode::Up => Action::NavigateUp,
        KeyCode::Char(c) => Action::InputChar(c),
        _ => Action::None,
    }
}

/// Normal mode: chars are commands.
fn translate_key(key: KeyEvent) -> Action {
    match (key.modifiers, key.code) {
        (KeyModifiers::NONE, KeyCode::Char('q')) => Action::Quit,
        (KeyModifiers::NONE, KeyCode::Char('j')) | (KeyModifiers::NONE, KeyCode::Down) => Action::NavigateDown,
        (KeyModifiers::NONE, KeyCode::Char('k')) | (KeyModifiers::NONE, KeyCode::Up) => Action::NavigateUp,
        (KeyModifiers::NONE, KeyCode::Char('h')) | (KeyModifiers::NONE, KeyCode::Left) => Action::NavigateLeft,
        (KeyModifiers::NONE, KeyCode::Char('l')) | (KeyModifiers::NONE, KeyCode::Right) => Action::NavigateRight,
        (KeyModifiers::NONE, KeyCode::Enter) => Action::Select,
        (KeyModifiers::NONE, KeyCode::Char('p')) => Action::AddProject,
        (KeyModifiers::NONE, KeyCode::Char('w')) => Action::AddWorktree,
        (KeyModifiers::NONE, KeyCode::Char('s')) => Action::AddSession,
        (KeyModifiers::NONE, KeyCode::Char('o')) => Action::OpenRun,
        (KeyModifiers::NONE, KeyCode::Char('d')) => Action::Delete,
        (KeyModifiers::NONE, KeyCode::Char('c')) => Action::Clean,
        (KeyModifiers::NONE, KeyCode::Char('e')) => Action::Edit,
        (KeyModifiers::NONE, KeyCode::Char('r')) => Action::SetAlias,
        (KeyModifiers::SHIFT, KeyCode::Char('R')) | (KeyModifiers::NONE, KeyCode::Char('R')) => Action::Refresh,
        (KeyModifiers::NONE, KeyCode::Char('?')) => Action::Help,
        (KeyModifiers::NONE, KeyCode::Char('y')) => Action::ConfirmYes,
        (KeyModifiers::NONE, KeyCode::Char('n')) => Action::NextAttention,
        (KeyModifiers::SHIFT, KeyCode::Char('N')) | (KeyModifiers::NONE, KeyCode::Char('N')) => Action::PrevAttention,
        (KeyModifiers::NONE, KeyCode::Char('x')) => Action::DismissAttention,
        (KeyModifiers::NONE, KeyCode::Char('m')) => Action::EnterMove,
        (KeyModifiers::CONTROL, KeyCode::Char('d')) => Action::JumpProjectDown,
        (KeyModifiers::CONTROL, KeyCode::Char('u')) => Action::JumpProjectUp,
        (KeyModifiers::NONE, KeyCode::Esc) => Action::InputEscape,
        (KeyModifiers::NONE, KeyCode::Backspace) => Action::InputBackspace,
        _ => Action::None,
    }
}
