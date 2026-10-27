//! One key dispatcher for the forms that edit text: the input prompt, the routine
//! editor and the global settings form. Each form keeps its own editing rules
//! behind this trait; the key mapping lives here once.

use super::global_settings::GlobalSettingsForm;
use super::input::InputState;
use super::routine_editor::RoutineForm;
use crate::action::Action;

pub(crate) trait TextForm {
    /// Whether a typed character edits text now. When false, the character is
    /// not handled here and a form may read it as a command key.
    fn accepts_text(&self) -> bool;
    fn insert(&mut self, c: char);
    fn backspace(&mut self);
    fn cursor_left(&mut self);
    fn cursor_right(&mut self);
    fn focus_next(&mut self);
    fn focus_prev(&mut self);
}

/// Applies a text-editing key to `form`. Returns false for every other action.
pub(crate) fn dispatch_text_edit(form: &mut impl TextForm, action: &Action) -> bool {
    match action {
        Action::InputChar(c) if form.accepts_text() => form.insert(*c),
        Action::InputBackspace => form.backspace(),
        Action::NavigateLeft => form.cursor_left(),
        Action::NavigateRight => form.cursor_right(),
        Action::InputTab | Action::NavigateDown => form.focus_next(),
        Action::InputBackTab | Action::NavigateUp => form.focus_prev(),
        _ => return false,
    }
    true
}

impl TextForm for InputState {
    fn accepts_text(&self) -> bool {
        true
    }
    fn insert(&mut self, c: char) {
        self.insert_char(c);
    }
    fn backspace(&mut self) {
        InputState::backspace(self);
    }
    fn cursor_left(&mut self) {
        InputState::cursor_left(self);
    }
    fn cursor_right(&mut self) {
        InputState::cursor_right(self);
    }
    fn focus_next(&mut self) {
        self.select_next();
    }
    fn focus_prev(&mut self) {
        self.select_prev();
    }
}

/// A routine form plus whether its name field may change. A fixed name takes no
/// typing or backspace, but the cursor and the focus still move.
pub(crate) struct RoutineTextEdit<'a> {
    pub form: &'a mut RoutineForm,
    pub can_rename: bool,
}

impl TextForm for RoutineTextEdit<'_> {
    fn accepts_text(&self) -> bool {
        self.can_rename || self.form.field != 0
    }
    fn insert(&mut self, c: char) {
        self.form.insert(c);
    }
    fn backspace(&mut self) {
        if self.accepts_text() {
            self.form.backspace();
        }
    }
    fn cursor_left(&mut self) {
        self.form.left();
    }
    fn cursor_right(&mut self) {
        self.form.right();
    }
    fn focus_next(&mut self) {
        self.form.next(false);
    }
    fn focus_prev(&mut self) {
        self.form.next(true);
    }
}

impl TextForm for GlobalSettingsForm {
    fn accepts_text(&self) -> bool {
        GlobalSettingsForm::accepts_text(self)
    }
    fn insert(&mut self, c: char) {
        GlobalSettingsForm::insert(self, c);
    }
    fn backspace(&mut self) {
        GlobalSettingsForm::backspace(self);
    }
    fn cursor_left(&mut self) {
        self.left();
    }
    fn cursor_right(&mut self) {
        self.right();
    }
    fn focus_next(&mut self) {
        self.next_field(false);
    }
    fn focus_prev(&mut self) {
        self.next_field(true);
    }
}
