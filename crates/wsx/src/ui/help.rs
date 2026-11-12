//! The `?` popup: a scrolling list of keys. The Workspace and the Tasks view each have their own.
use crate::action::Action;
use crate::app::{App, Mode};
use crate::ui::{popup_block, popup_center, theme};
use ratatui::{
    prelude::*,
    widgets::{Clear, Paragraph},
};
use std::cell::Cell;

/// Scroll of the open popup; `max_scroll` and `page` are what the last frame showed.
#[derive(Debug, Clone, Default)]
pub struct HelpView {
    pub scroll: usize,
    max_scroll: Cell<usize>,
    page: Cell<usize>,
}

impl HelpView {
    /// One key; false when the popup closes. `j/k`, PgUp/PgDn, `g` top and `G` bottom scroll.
    pub fn key(&mut self, action: &Action) -> bool {
        let (max, page) = (self.max_scroll.get(), self.page.get().max(1));
        match action {
            Action::InputEscape | Action::Quit | Action::Help | Action::InputChar('?') => {
                return false
            }
            Action::NavigateDown | Action::InputChar('j') => self.scroll += 1,
            Action::NavigateUp | Action::InputChar('k') => {
                self.scroll = self.scroll.saturating_sub(1)
            }
            Action::PageDown => self.scroll += page,
            Action::PageUp => self.scroll = self.scroll.saturating_sub(page),
            Action::AssignGroup | Action::InputChar('g') => self.scroll = 0,
            Action::JumpEnd | Action::InputChar('G') => self.scroll = max,
            _ => {}
        }
        self.scroll = self.scroll.min(max);
        true
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Topic {
    Workspace,
    Tasks,
}

const WORKSPACE: &[&str] = &[
    " Navigation",
    "  j/k / ↑↓     Navigate tree",
    "  h/l / ←→     Collapse/expand",
    "  Enter         Project/Worktree: toggle  |  Session: Terminal mode",
    "  /             Search sessions and files",
    "",
    " Project",
    "  p             Add project (path: prompt)",
    "  u             Create routine",
    "  m             Move project (reorder list)",
    "  g             Assign project group",
    "  d             Unregister project (H: also halt its sessions)",
    "  e             View/edit wsx.config.yml",
    "  t             Tasks (Enter detail, m chat, S shell,",
    "                o view, u usage, a agent; ? lists its keys)",
    "  v / M         Panels (M: this Mac with the machine panels)",
    "",
    " Worktree",
    "  w             Add worktree (branch: prompt)",
    "  s             New persistent session (optional init command)",
    "  r             Set alias",
    "  m             Move worktree (reorder project list)",
    "  d             Delete worktree + kill all sessions",
    "  e             View/edit wsx.config.yml",
    "",
    " Session",
    "  Enter         Enter Terminal mode",
    "  C             Send Ctrl+C to session",
    "  |             Split the pane right",
    "  -             Split the pane down",
    "  r             Rename",
    "  m             Move session (reorder worktree list)",
    "  d             Kill session",
    "  x             Acknowledge done, otherwise toggle ⊘ mute",
    "",
    " Routine",
    "  Enter         Run history",
    "  r             Run it now",
    "  Space         Turn it on / off",
    "  e             Edit",
    "  d             Delete",
    "",
    " Terminal mode",
    "$terminal_escape",
    "$terminal_quit",
    "$terminal_session",
    "$terminal_group",
    "$terminal_idle",
    "$terminal_active",
    "$terminal_attention",
    "$terminal_sidebar",
    "$terminal_literal",
    "",
    " Groups (optional)",
    "  T             Open scrollable Groups sidebar",
    "  { / }         Switch to previous/next group",
    "  g             Assign selected project to a group",
    "  a/r/d, J/K    Add, rename, delete, reorder in Groups",
    "  P             Next profile (with [[profiles]] in the config)",
    "",
    " Global",
    "  [ / ]         Jump to prev / next project",
    "  i / I         Jump to next / prev idle session (○)",
    "  a / A         Jump to next / prev active session (◉)",
    "  n / N         Jump to next / prev attention session (◐ ✓)",
    "  R             Refresh",
    "  ,             Open global settings",
    "  ?             Help (j/k, PgUp/PgDn, g/G scroll)",
    "  q             Quit TUI",
    "  Q             Hard quit TUI and wsxd",
];

const TASKS: &[&str] = &[
    " Tasks list",
    "  j/k / ↑↓     Move",
    "  Enter         Task detail (a chat: opens it)",
    "  m             Manager chat",
    "  S             Shell in the project",
    "  a             Pick the agent the spend counts",
    "  u             Usage: cost or tokens",
    "  o             Project overview",
    "  Esc / t / q   Close",
    "",
    " Task detail",
    "  j/k           Scroll",
    "  S             Shell in the task's worktree",
    "  L             Land (a live trial: pass it); asks first",
    "  a             Answer: the manager chat, starting #n",
    "  H             Take over, or hand back; asks first",
    "  T             End the trial, else try it on a preview",
    "  Esc / Enter   Back",
    "",
    " Chat (list focus)",
    "  i / Enter     Write a message",
    "  j/k / ↑↓     Scroll a line",
    "  PgUp / PgDn   Scroll a page",
    "  g / G         Oldest loaded row / newest",
    "  /             Find (n older, N newer)",
    "  [ / ]         Select the previous / next link; Enter opens it",
    "  P             Chat of the other manager whose message is nearest",
    "                the bottom of the view (no message cursor)",
    "  m             Media list",
    "  o             Project overview",
    "  S             Shell",
    "  Tab           Next item to answer",
    "  p / s / f     To check: pass, skip, fail with a note",
    "  y / n         Approval: approve, deny",
    "  r / d         Question: reply, dismiss",
    "  Esc           Back",
    "",
    " Message line",
    "  Enter         Send",
    "  Esc           Back to the chat",
    "  ?             Types a question mark",
    "  paste         Inserts the text",
];

/// The popup over what shows now, when the Workspace or the Tasks view has it open.
pub fn render(frame: &mut Frame, area: Rect, app: &App) {
    let (topic, view) = match &app.mode {
        Mode::Help(view) => (Topic::Workspace, view),
        Mode::Tasks { list } => match &list.help {
            Some(view) => (Topic::Tasks, view),
            None => return,
        },
        _ => return,
    };
    let width = area.width.clamp(40, 64);
    let height = area.height.clamp(12, 42);
    let popup = popup_center(area, width, height);

    frame.render_widget(Clear, popup);

    let entries = match topic {
        Topic::Workspace => WORKSPACE,
        Topic::Tasks => TASKS,
    };
    let inner_width = (width as usize).saturating_sub(2);
    let lines: Vec<Line> = entries
        .iter()
        .flat_map(|entry| {
            let text = match *entry {
                "$terminal_escape" => Some(format!(
                    "  {:<14} Focus Workspace",
                    app.terminal_escape_label()
                )),
                "$terminal_quit" => app
                    .terminal_quit_label()
                    .map(|label| format!("  {label:<14} Quit TUI")),
                "$terminal_session" => app.terminal_quit_label().map(|_| {
                    format!(
                        "  Prefix+{} or ←/→  Previous / next project session",
                        app.terminal_session_navigation_label()
                    )
                }),
                "$terminal_group" => app
                    .terminal_quit_label()
                    .map(|_| "  Prefix+{ / }  Previous / next group target".into()),
                "$terminal_idle" => app
                    .terminal_quit_label()
                    .map(|_| "  Prefix+I / i  Previous / next idle session".into()),
                "$terminal_active" => app
                    .terminal_quit_label()
                    .map(|_| "  Prefix+A / a  Previous / next active session".into()),
                "$terminal_attention" => app
                    .terminal_quit_label()
                    .map(|_| "  Prefix+N / n  Previous / next session needing attention".into()),
                "$terminal_sidebar" => app
                    .terminal_sidebar_hint()
                    .map(|_| "  Prefix+B      Toggle Terminal sidebar".into()),
                "$terminal_literal" => Some(format!(
                    "  {:<14} Send literal prefix",
                    app.terminal_literal_escape_label()
                )),
                other => Some(other.to_string()),
            };
            text.map_or_else(Vec::new, |text| help_wrap_line(&text, inner_width))
        })
        .collect();

    // ^ The popup shows `rows` lines of `lines`; the keys read these two from the last frame.
    let rows = usize::from(height).saturating_sub(2);
    let max_scroll = lines.len().saturating_sub(rows);
    view.max_scroll.set(max_scroll);
    view.page.set(rows.saturating_sub(1));
    let first = view.scroll.min(max_scroll);
    let hint = if max_scroll > 0 {
        " j/k PgUp/PgDn g/G scroll · Esc close "
    } else {
        " Esc close "
    };

    let block = popup_block(
        Line::from(match topic {
            Topic::Workspace => " Help ",
            Topic::Tasks => " Help: Tasks ",
        }),
        Line::styled(hint, Style::default().fg(theme::TEXT_MUTED)),
        Style::default().fg(theme::ACCENT),
    );
    let para = Paragraph::new(lines).block(block).scroll((first as u16, 0));
    frame.render_widget(para, popup);
}

/// Wrap a help entry, indenting continuation lines to align with the description column.
fn help_wrap_line(line: &str, width: usize) -> Vec<Line<'static>> {
    // Find where the description starts: first run of 2+ spaces after a non-space char
    // (following the 2-char indent). Key lines look like "  key     description".
    let desc_col = if line.starts_with("  ") && !line[2..].starts_with(' ') {
        let rest = &line[2..];
        let mut found = None;
        let mut in_spaces = false;
        let mut space_start = 0;
        for (i, c) in rest.char_indices() {
            if c == ' ' {
                if !in_spaces {
                    space_start = i;
                    in_spaces = true;
                }
            } else {
                if in_spaces && i - space_start >= 2 {
                    found = Some(i);
                    break;
                }
                in_spaces = false;
            }
        }
        found.map(|i| 2 + i) // byte offset of description start
    } else {
        None
    };

    let Some(desc_byte) = desc_col else {
        return vec![Line::from(line.to_owned())];
    };

    // Measure key column display width (chars, treating all as 1-wide)
    let key_display: usize = line[..desc_byte].chars().count();
    let desc_text = &line[desc_byte..];
    let desc_width = width.saturating_sub(key_display);

    if desc_text.len() <= desc_width {
        return vec![Line::from(line.to_owned())];
    }

    // Word-wrap the description
    let indent = " ".repeat(key_display);
    let key_part = line[..desc_byte].to_owned();
    let mut result = Vec::new();
    let mut remaining = desc_text;
    let mut first = true;

    while !remaining.is_empty() {
        let avail = if first {
            desc_width
        } else {
            width.saturating_sub(key_display)
        };
        let (chunk, rest) = split_at_word(remaining, avail);
        if first {
            result.push(Line::from(format!("{}{}", key_part, chunk)));
            first = false;
        } else {
            result.push(Line::from(format!("{}{}", indent, chunk)));
        }
        remaining = rest.trim_start();
    }
    result
}

/// Split `s` at a word boundary no longer than `max_chars`. Returns (chunk, remainder).
fn split_at_word(s: &str, max_chars: usize) -> (&str, &str) {
    let char_count = s.chars().count();
    if char_count <= max_chars {
        return (s, "");
    }
    // Find byte offset of max_chars-th char
    let end_byte = s
        .char_indices()
        .nth(max_chars)
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    // Walk back to last space
    if let Some(space) = s[..end_byte].rfind(' ') {
        (&s[..space], &s[space..])
    } else {
        (&s[..end_byte], &s[end_byte..])
    }
}
