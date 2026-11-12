//! The Tasks view's New chat form (`n`): a name, an optional goal and an `[[agents]]` entry, as
//! the web's New chat sheet. A name that is empty, bad or open is refused in the form, live.
//! ref: docs/tasks.md "TUI", "Chats"
use crate::action::Action;
use crate::ui::input::InputState;
use wsx_core::task::chats;

/// What a key did to the form.
pub enum Outcome {
    Open,
    Cancel,
    /// Enter: the chat made, which opens.
    Done(String),
}

pub struct NewChat {
    pub name: InputState,
    pub goal: InputState,
    /// The `[[agents]]` entries a chat may run on, and the one picked (none: the manager's).
    pub agents: Vec<(String, chats::ChatAgent)>,
    pub agent: Option<usize>,
    /// 0 name, 1 goal, 2 agent (only with entries).
    pub focus: usize,
    /// The last refusal of the store, which stays until the next key.
    pub error: Option<String>,
    /// The project's open chats, whose names are taken.
    taken: Vec<String>,
    /// The manager's agent in few words, which `default` runs on.
    default_agent: Option<String>,
    /// Enter was pressed with no name.
    asked: bool,
}

impl NewChat {
    pub fn new(project_id: &str, default_agent: Option<String>) -> Self {
        Self {
            name: InputState::new("name: "),
            goal: InputState::new("goal: "),
            agents: chats::agent_entries(),
            agent: None,
            focus: 0,
            error: None,
            taken: chats::open_names(project_id),
            default_agent,
            asked: false,
        }
    }

    /// The line under the fields: the store's refusal, else why the name is refused, else the
    /// name's rule; the bool is whether it is a refusal.
    pub fn note(&self) -> (String, bool) {
        let name = self.name.buffer.trim();
        if let Some(error) = &self.error {
            (error.clone(), true)
        } else if let Some(problem) = chats::name_problem(name, &self.taken) {
            (problem, true)
        } else if self.asked && name.is_empty() {
            ("name: required".into(), true)
        } else {
            ("name: a-z, 0-9 and -, up to 24".into(), false)
        }
    }

    /// How many fields show: the agent only when the config has entries.
    pub fn agents_rows(&self) -> usize {
        if self.agents.is_empty() {
            2
        } else {
            3
        }
    }

    /// The agent picked, as `name (model)`.
    pub fn agent_words(&self) -> String {
        self.agent.and_then(|at| self.agents.get(at)).map_or_else(
            || {
                self.default_agent
                    .as_ref()
                    .map_or("default".into(), |agent| format!("default ({agent})"))
            },
            |(name, agent)| format!("{name} ({})", agent.model),
        )
    }

    fn cycle(&mut self, step: isize) {
        // ^ The default is the slot before the first entry.
        let slots = self.agents.len() as isize + 1;
        let at = self.agent.map_or(0, |at| at as isize + 1);
        let next = (at + step).rem_euclid(slots);
        self.agent = (next > 0).then(|| next as usize - 1);
    }

    /// One key. `Done` when `Enter` made the chat, after `create` answered; a refusal stays in
    /// the note and the form stays.
    pub fn key(&mut self, action: &Action, project_id: &str) -> Outcome {
        self.error = None;
        // ^ The name is typed as the web sheet takes it: lower case, a space as `-`.
        let name_field = self.focus == 0;
        let typed = |c: char| match c {
            ' ' if name_field => '-',
            c if name_field => c.to_ascii_lowercase(),
            c => c,
        };
        let field = match self.focus {
            0 => Some(&mut self.name),
            1 => Some(&mut self.goal),
            _ => None,
        };
        match (action, field) {
            (Action::InputEscape, _) => return Outcome::Cancel,
            (Action::Select, _) => return self.submit(project_id),
            (Action::InputTab | Action::NavigateDown, _) => {
                self.focus = (self.focus + 1) % self.agents_rows()
            }
            (Action::InputBackTab | Action::NavigateUp, _) => {
                self.focus = (self.focus + self.agents_rows() - 1) % self.agents_rows()
            }
            (Action::InputChar(c), Some(field)) => field.insert_char(typed(*c)),
            (Action::Paste(text), Some(field)) => text
                .lines()
                .next()
                .unwrap_or_default()
                .chars()
                .for_each(|c| field.insert_char(typed(c))),
            (Action::InputBackspace, Some(field)) => field.backspace(),
            (Action::NavigateLeft, Some(field)) => field.cursor_left(),
            (Action::NavigateRight, Some(field)) => field.cursor_right(),
            (Action::InputChar(' ') | Action::NavigateRight, None) => self.cycle(1),
            (Action::NavigateLeft, None) => self.cycle(-1),
            _ => {}
        }
        Outcome::Open
    }

    fn submit(&mut self, project_id: &str) -> Outcome {
        let name = self.name.buffer.trim().to_string();
        self.asked = true;
        if name.is_empty() || chats::name_problem(&name, &self.taken).is_some() {
            self.focus = 0;
            return Outcome::Open;
        }
        let goal = self.goal.buffer.trim();
        let agent = self
            .agent
            .and_then(|at| self.agents.get(at))
            .map(|(name, _)| name.as_str());
        match chats::create(project_id, &name, (!goal.is_empty()).then_some(goal), agent) {
            Ok(_) => Outcome::Done(name),
            Err(error) => {
                self.error = Some(format!("{error:#}"));
                Outcome::Open
            }
        }
    }
}
