// Categorized global settings editor.
// ^ [[Configuration Model]] Typed controls edit a draft; only validated drafts reach GlobalConfig.

use std::collections::BTreeSet;

use ratatui::{
    prelude::*,
    widgets::{Clear, List, ListItem, ListState, Paragraph, Wrap},
};
use wsx_core::config::global::{GlobalConfig, PortVisibility, TerminalSidebar};

use super::{popup_block, popup_center, theme};

#[cfg(target_os = "macos")]
const RUNTIME_FIELDS: &[SettingField] = &[SettingField::ResumeAgents, SettingField::WakeMode];
#[cfg(not(target_os = "macos"))]
const RUNTIME_FIELDS: &[SettingField] = &[SettingField::ResumeAgents];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsCategory {
    Workspace,
    View,
    Terminal,
    Runtime,
}

impl SettingsCategory {
    const ALL: [Self; 4] = [Self::Workspace, Self::View, Self::Terminal, Self::Runtime];

    fn label(self) -> &'static str {
        match self {
            Self::Workspace => "Workspace",
            Self::View => "View",
            Self::Terminal => "Terminal",
            Self::Runtime => "Runtime",
        }
    }

    fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|candidate| *candidate == self)
            .unwrap_or(0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingField {
    AutoCollapse,
    ExcludedPaths,
    ShowRelease,
    PortVisibility,
    NotificationTimeout,
    PrefixModifier,
    PrefixKey,
    WorkspaceKey,
    TerminalSidebar,
    ResumeAgents,
    WakeMode,
}

impl SettingField {
    fn for_category(category: SettingsCategory) -> &'static [Self] {
        match category {
            SettingsCategory::Workspace => &[Self::AutoCollapse, Self::ExcludedPaths],
            SettingsCategory::View => &[
                Self::ShowRelease,
                Self::PortVisibility,
                Self::NotificationTimeout,
            ],
            SettingsCategory::Terminal => &[
                Self::PrefixModifier,
                Self::PrefixKey,
                Self::WorkspaceKey,
                Self::TerminalSidebar,
            ],
            SettingsCategory::Runtime => RUNTIME_FIELDS,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::AutoCollapse => "Automatic collapse",
            Self::ExcludedPaths => "Excluded worktree paths",
            Self::ShowRelease => "Release status",
            Self::PortVisibility => "Session ports",
            Self::NotificationTimeout => "Notification timeout",
            Self::PrefixModifier => "Prefix modifier",
            Self::PrefixKey => "Prefix key",
            Self::WorkspaceKey => "Workspace key",
            Self::TerminalSidebar => "Terminal sidebar",
            Self::ResumeAgents => "Resume agents",
            Self::WakeMode => "Wake mode",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::AutoCollapse => "Hours before inactive projects collapse. Use 0 to disable.",
            Self::ExcludedPaths => "Path fragments ignored during worktree discovery.",
            Self::ShowRelease => "Show the running version and available update in the footer.",
            Self::PortVisibility => "Control ports in session rows and terminal breadcrumbs. Branch detail always shows ports.",
            Self::NotificationTimeout => "Seconds before success, warning, and error notices expire.",
            Self::PrefixModifier => "Modifier combination used by the terminal prefix.",
            Self::PrefixKey => "One key combined with the selected modifier, for example Ctrl+A.",
            Self::WorkspaceKey => "Key pressed after the prefix to focus Workspace. The reserved q suffix quits.",
            Self::TerminalSidebar => "Use a two-column status rail or the full Workspace tree in Terminal mode.",
            Self::ResumeAgents => "Resume saved agent commands when wsxd starts again.",
            Self::WakeMode => "Prevent idle system sleep while an agent is actively working.",
        }
    }
}

#[derive(Debug, Clone)]
struct TextEditor {
    value: String,
    cursor: usize,
}

impl TextEditor {
    fn new(value: String) -> Self {
        let cursor = value.len();
        Self { value, cursor }
    }

    fn insert(&mut self, character: char) {
        self.value.insert(self.cursor, character);
        self.cursor += character.len_utf8();
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let previous = self.value[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(index, _)| index)
            .unwrap_or(0);
        self.value.drain(previous..self.cursor);
        self.cursor = previous;
    }

    fn left(&mut self) {
        self.cursor = self.value[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(index, _)| index)
            .unwrap_or(0);
    }

    fn right(&mut self) {
        if let Some(character) = self.value[self.cursor..].chars().next() {
            self.cursor += character.len_utf8();
        }
    }
}

#[derive(Debug, Clone)]
struct ChoiceEditor {
    selected: usize,
    labels: Vec<&'static str>,
}

#[derive(Debug, Clone)]
struct ListEditor {
    values: Vec<String>,
    selected: usize,
    marked: BTreeSet<usize>,
    text: Option<(Option<usize>, TextEditor)>,
}

#[derive(Debug, Clone)]
enum FieldEditor {
    Text(TextEditor),
    Choice(ChoiceEditor),
    List(ListEditor),
}

#[derive(Debug, Clone)]
pub struct GlobalSettingsForm {
    original: GlobalConfig,
    draft: GlobalConfig,
    category: SettingsCategory,
    field: usize,
    editor: Option<FieldEditor>,
}

impl GlobalSettingsForm {
    pub fn new(config: GlobalConfig) -> Self {
        Self {
            original: config.clone(),
            draft: config,
            category: SettingsCategory::Workspace,
            field: 0,
            editor: None,
        }
    }

    pub fn draft(&self) -> &GlobalConfig {
        &self.draft
    }

    pub fn is_dirty(&self) -> bool {
        self.original != self.draft
    }

    pub fn is_editing(&self) -> bool {
        self.editor.is_some()
    }

    pub fn accepts_text(&self) -> bool {
        matches!(self.editor, Some(FieldEditor::Text(_)))
            || matches!(
                self.editor,
                Some(FieldEditor::List(ListEditor { text: Some(_), .. }))
            )
    }

    fn selected_field(&self) -> SettingField {
        SettingField::for_category(self.category)[self.field]
    }

    pub fn next_category(&mut self, backwards: bool) {
        if self.editor.is_some() {
            return;
        }
        let current = self.category.index();
        let next = if backwards {
            (current + SettingsCategory::ALL.len() - 1) % SettingsCategory::ALL.len()
        } else {
            (current + 1) % SettingsCategory::ALL.len()
        };
        self.category = SettingsCategory::ALL[next];
        self.field = 0;
    }

    pub fn next_field(&mut self, backwards: bool) {
        match self.editor.as_mut() {
            Some(FieldEditor::Choice(editor)) => {
                let len = editor.labels.len();
                editor.selected = if backwards {
                    (editor.selected + len - 1) % len
                } else {
                    (editor.selected + 1) % len
                };
            }
            Some(FieldEditor::List(editor)) if editor.text.is_none() => {
                if !editor.values.is_empty() {
                    editor.selected = if backwards {
                        editor.selected.saturating_sub(1)
                    } else {
                        (editor.selected + 1).min(editor.values.len() - 1)
                    };
                }
            }
            Some(_) => {}
            None => {
                let len = SettingField::for_category(self.category).len();
                self.field = if backwards {
                    (self.field + len - 1) % len
                } else {
                    (self.field + 1) % len
                };
            }
        }
    }

    pub fn left(&mut self) {
        match self.editor.as_mut() {
            Some(FieldEditor::Text(editor)) => editor.left(),
            Some(FieldEditor::List(ListEditor {
                text: Some((_, editor)),
                ..
            })) => editor.left(),
            Some(_) => {}
            None => self.next_category(true),
        }
    }

    pub fn right(&mut self) {
        match self.editor.as_mut() {
            Some(FieldEditor::Text(editor)) => editor.right(),
            Some(FieldEditor::List(ListEditor {
                text: Some((_, editor)),
                ..
            })) => editor.right(),
            Some(_) => {}
            None => self.next_category(false),
        }
    }

    pub fn insert(&mut self, character: char) {
        match self.editor.as_mut() {
            Some(FieldEditor::Text(editor)) => editor.insert(character),
            Some(FieldEditor::List(ListEditor {
                text: Some((_, editor)),
                ..
            })) => editor.insert(character),
            _ => {}
        }
    }

    pub fn backspace(&mut self) {
        match self.editor.as_mut() {
            Some(FieldEditor::Text(editor)) => editor.backspace(),
            Some(FieldEditor::List(ListEditor {
                text: Some((_, editor)),
                ..
            })) => editor.backspace(),
            _ => {}
        }
    }

    pub fn begin_or_commit(&mut self) -> Result<(), String> {
        if self.editor.is_none() {
            self.editor = Some(self.editor_for_selected());
            return Ok(());
        }
        if let Some(FieldEditor::List(editor)) = self.editor.as_mut() {
            if let Some((index, text)) = editor.text.as_ref() {
                let value = text.value.trim().to_string();
                if value.is_empty() {
                    return Err("path fragment cannot be empty".into());
                }
                let index = *index;
                editor.text = None;
                if let Some(index) = index {
                    editor.values[index] = value;
                } else if !editor.values.contains(&value) {
                    editor.values.push(value);
                    editor.selected = editor.values.len() - 1;
                }
                return Ok(());
            }
        }
        let field = self.selected_field();
        let editor = self.editor.take().expect("editor checked above");
        let rollback = editor.clone();
        let result = match (field, editor) {
            (SettingField::AutoCollapse, FieldEditor::Text(editor)) => {
                parse_u64(&editor.value, true).map(|value| {
                    self.draft.auto_collapse_after_hours = value;
                })
            }
            (SettingField::NotificationTimeout, FieldEditor::Text(editor)) => {
                parse_u64(&editor.value, false).map(|value| {
                    self.draft.notification_timeout_seconds = value;
                })
            }
            (SettingField::PrefixModifier, FieldEditor::Choice(editor)) => {
                let mut binding = current_escape_binding(&self.draft);
                binding.modifier = editor.selected;
                store_escape_binding(&mut self.draft, &binding);
                Ok(())
            }
            (SettingField::PrefixKey, FieldEditor::Text(editor)) => {
                normalize_binding_key(&editor.value, false)
                    .ok_or_else(|| "prefix key must be one key, Space, Tab, or Esc".to_string())
                    .map(|key| {
                        let mut binding = current_escape_binding(&self.draft);
                        binding.prefix_key = key;
                        store_escape_binding(&mut self.draft, &binding);
                    })
            }
            (SettingField::WorkspaceKey, FieldEditor::Text(editor)) => {
                normalize_binding_key(&editor.value, true)
                    .ok_or_else(|| {
                        "workspace key must be one key other than reserved q".to_string()
                    })
                    .map(|key| {
                        let mut binding = current_escape_binding(&self.draft);
                        binding.workspace_key = key;
                        store_escape_binding(&mut self.draft, &binding);
                    })
            }
            (SettingField::ShowRelease, FieldEditor::Choice(editor)) => {
                self.draft.show_release_status = editor.selected == 0;
                Ok(())
            }
            (SettingField::TerminalSidebar, FieldEditor::Choice(editor)) => {
                self.draft.terminal_sidebar = if editor.selected == 0 {
                    TerminalSidebar::Compact
                } else {
                    TerminalSidebar::Expanded
                };
                Ok(())
            }
            (SettingField::ResumeAgents, FieldEditor::Choice(editor)) => {
                self.draft.resume_agents_on_restore = editor.selected == 0;
                Ok(())
            }
            (SettingField::WakeMode, FieldEditor::Choice(editor)) => {
                self.draft.wake_mode = editor.selected == 0;
                Ok(())
            }
            (SettingField::PortVisibility, FieldEditor::Choice(editor)) => {
                self.draft.port_visibility = match editor.selected {
                    0 => PortVisibility::Hidden,
                    1 => PortVisibility::NonAgentic,
                    _ => PortVisibility::All,
                };
                Ok(())
            }
            (SettingField::ExcludedPaths, FieldEditor::List(editor)) => {
                self.draft.exclude_worktree_paths = editor.values;
                Ok(())
            }
            _ => Err("setting editor type mismatch".into()),
        };
        if result.is_err() {
            self.editor = Some(rollback);
        }
        result
    }

    fn editor_for_selected(&self) -> FieldEditor {
        match self.selected_field() {
            SettingField::AutoCollapse => FieldEditor::Text(TextEditor::new(
                self.draft.auto_collapse_after_hours.to_string(),
            )),
            SettingField::NotificationTimeout => FieldEditor::Text(TextEditor::new(
                self.draft.notification_timeout_seconds.to_string(),
            )),
            SettingField::PrefixModifier => {
                let binding = current_escape_binding(&self.draft);
                FieldEditor::Choice(ChoiceEditor {
                    selected: binding.modifier,
                    labels: MODIFIER_LABELS.to_vec(),
                })
            }
            SettingField::PrefixKey => FieldEditor::Text(TextEditor::new(
                current_escape_binding(&self.draft).prefix_key,
            )),
            SettingField::WorkspaceKey => FieldEditor::Text(TextEditor::new(
                current_escape_binding(&self.draft).workspace_key,
            )),
            SettingField::ShowRelease => FieldEditor::Choice(ChoiceEditor {
                selected: usize::from(!self.draft.show_release_status),
                labels: vec!["On", "Off"],
            }),
            SettingField::TerminalSidebar => FieldEditor::Choice(ChoiceEditor {
                selected: usize::from(self.draft.terminal_sidebar == TerminalSidebar::Expanded),
                labels: vec!["Compact", "Expanded"],
            }),
            SettingField::ResumeAgents => FieldEditor::Choice(ChoiceEditor {
                selected: usize::from(!self.draft.resume_agents_on_restore),
                labels: vec!["On", "Off"],
            }),
            SettingField::WakeMode => FieldEditor::Choice(ChoiceEditor {
                selected: usize::from(!self.draft.wake_mode),
                labels: vec!["On", "Off"],
            }),
            SettingField::PortVisibility => FieldEditor::Choice(ChoiceEditor {
                selected: match self.draft.port_visibility {
                    PortVisibility::Hidden => 0,
                    PortVisibility::NonAgentic => 1,
                    PortVisibility::All => 2,
                },
                labels: vec!["Hidden", "Non-agentic only", "All"],
            }),
            SettingField::ExcludedPaths => FieldEditor::List(ListEditor {
                values: self.draft.exclude_worktree_paths.clone(),
                selected: 0,
                marked: BTreeSet::new(),
                text: None,
            }),
        }
    }

    pub fn cancel_editor(&mut self) -> bool {
        let Some(editor) = self.editor.as_mut() else {
            return false;
        };
        if let FieldEditor::List(editor) = editor {
            if editor.text.take().is_some() {
                return true;
            }
        }
        self.editor = None;
        true
    }

    pub fn toggle(&mut self) {
        match self.editor.as_mut() {
            Some(FieldEditor::List(editor))
                if editor.text.is_none() && !editor.values.is_empty() =>
            {
                if !editor.marked.remove(&editor.selected) {
                    editor.marked.insert(editor.selected);
                }
            }
            None => match self.selected_field() {
                SettingField::ShowRelease => {
                    self.draft.show_release_status = !self.draft.show_release_status
                }
                SettingField::ResumeAgents => {
                    self.draft.resume_agents_on_restore = !self.draft.resume_agents_on_restore
                }
                SettingField::WakeMode => self.draft.wake_mode = !self.draft.wake_mode,
                _ => {}
            },
            _ => {}
        }
    }

    pub fn add_list_item(&mut self) {
        if let Some(FieldEditor::List(editor)) = self.editor.as_mut() {
            if editor.text.is_none() {
                editor.text = Some((None, TextEditor::new(String::new())));
            }
        }
    }

    pub fn edit_list_item(&mut self) {
        if let Some(FieldEditor::List(editor)) = self.editor.as_mut() {
            if editor.text.is_none() && !editor.values.is_empty() {
                editor.text = Some((
                    Some(editor.selected),
                    TextEditor::new(editor.values[editor.selected].clone()),
                ));
            }
        }
    }

    pub fn delete_list_items(&mut self) {
        if let Some(FieldEditor::List(editor)) = self.editor.as_mut() {
            if editor.text.is_some() || editor.values.is_empty() {
                return;
            }
            let removed = if editor.marked.is_empty() {
                BTreeSet::from([editor.selected])
            } else {
                std::mem::take(&mut editor.marked)
            };
            editor.values = editor
                .values
                .drain(..)
                .enumerate()
                .filter_map(|(index, value)| (!removed.contains(&index)).then_some(value))
                .collect();
            editor.selected = editor.selected.min(editor.values.len().saturating_sub(1));
        }
    }

    pub fn reset_saved(&mut self, config: GlobalConfig) {
        self.original = config.clone();
        self.draft = config;
        self.editor = None;
    }
}

fn parse_u64(value: &str, allow_zero: bool) -> Result<u64, String> {
    let parsed = value
        .trim()
        .parse::<u64>()
        .map_err(|_| "value must be a positive whole number".to_string())?;
    if !allow_zero && parsed == 0 {
        return Err("value must be at least 1".into());
    }
    Ok(parsed)
}

const MODIFIER_LABELS: [&str; 15] = [
    "Ctrl",
    "Alt",
    "Shift",
    "Super",
    "Ctrl+Alt",
    "Ctrl+Shift",
    "Ctrl+Super",
    "Alt+Shift",
    "Alt+Super",
    "Shift+Super",
    "Ctrl+Alt+Shift",
    "Ctrl+Alt+Super",
    "Ctrl+Shift+Super",
    "Alt+Shift+Super",
    "Ctrl+Alt+Shift+Super",
];

#[derive(Debug, Clone, PartialEq, Eq)]
struct EscapeBindingDraft {
    modifier: usize,
    prefix_key: String,
    workspace_key: String,
}

fn escape_binding(value: &str) -> Option<EscapeBindingDraft> {
    let mut sequence = value.split_whitespace();
    let prefix = sequence.next()?;
    let workspace_key = sequence.next().unwrap_or("w");
    if sequence.next().is_some() {
        return None;
    }
    let mut parts = prefix.split('+').collect::<Vec<_>>();
    let prefix_key = normalize_binding_key(parts.pop()?, false)?;
    let mut modifiers = parts
        .into_iter()
        .map(|part| match part.trim().to_ascii_lowercase().as_str() {
            "ctrl" | "control" => Some("Ctrl"),
            "alt" => Some("Alt"),
            "shift" => Some("Shift"),
            "super" | "cmd" => Some("Super"),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    modifiers.sort_by_key(|modifier| {
        ["Ctrl", "Alt", "Shift", "Super"]
            .iter()
            .position(|candidate| candidate == modifier)
            .unwrap_or(usize::MAX)
    });
    modifiers.dedup();
    let modifier_label = modifiers.join("+");
    let modifier = MODIFIER_LABELS
        .iter()
        .position(|candidate| *candidate == modifier_label)?;
    Some(EscapeBindingDraft {
        modifier,
        prefix_key,
        workspace_key: normalize_binding_key(workspace_key, true)?,
    })
}

fn normalize_binding_key(value: &str, reserve_quit: bool) -> Option<String> {
    let value = value.trim().to_ascii_lowercase();
    let normalized = match value.as_str() {
        "space" => "space".to_string(),
        "tab" => "tab".to_string(),
        "escape" | "esc" => "esc".to_string(),
        _ if value.chars().count() == 1 && !value.chars().any(char::is_whitespace) => value,
        _ => return None,
    };
    (!reserve_quit || normalized != "q").then_some(normalized)
}

fn current_escape_binding(config: &GlobalConfig) -> EscapeBindingDraft {
    escape_binding(&config.terminal_escape_chord)
        .or_else(|| escape_binding("ctrl+a w"))
        .expect("default terminal binding must be valid")
}

fn store_escape_binding(config: &mut GlobalConfig, binding: &EscapeBindingDraft) {
    config.terminal_escape_chord = format!(
        "{}+{} {}",
        MODIFIER_LABELS[binding.modifier].to_ascii_lowercase(),
        binding.prefix_key,
        binding.workspace_key
    );
}

fn setting_value(form: &GlobalSettingsForm, field: SettingField) -> String {
    if form.selected_field() == field {
        match form.editor.as_ref() {
            Some(FieldEditor::Text(editor)) => return format!("{}▏", editor.value),
            Some(FieldEditor::Choice(editor)) => {
                return format!("‹ {} ›", editor.labels[editor.selected])
            }
            Some(FieldEditor::List(editor)) => {
                if let Some((_, text)) = &editor.text {
                    return format!("{}▏", text.value);
                }
                if editor.values.is_empty() {
                    return "(empty)".into();
                }
                let marker = if editor.marked.contains(&editor.selected) {
                    "[x]"
                } else {
                    "[ ]"
                };
                return format!(
                    "{marker} {}  {}/{}",
                    editor.values[editor.selected],
                    editor.selected + 1,
                    editor.values.len()
                );
            }
            None => {}
        }
    }
    match field {
        SettingField::AutoCollapse => format!("{} hours", form.draft.auto_collapse_after_hours),
        SettingField::ExcludedPaths => {
            format!("{} entries", form.draft.exclude_worktree_paths.len())
        }
        SettingField::ShowRelease => on_off(form.draft.show_release_status).into(),
        SettingField::PortVisibility => match form.draft.port_visibility {
            PortVisibility::Hidden => "Hidden".into(),
            PortVisibility::NonAgentic => "Non-agentic only".into(),
            PortVisibility::All => "All".into(),
        },
        SettingField::NotificationTimeout => {
            format!("{} seconds", form.draft.notification_timeout_seconds)
        }
        SettingField::PrefixModifier => {
            MODIFIER_LABELS[current_escape_binding(&form.draft).modifier].into()
        }
        SettingField::PrefixKey => current_escape_binding(&form.draft).prefix_key,
        SettingField::WorkspaceKey => current_escape_binding(&form.draft).workspace_key,
        SettingField::TerminalSidebar => match form.draft.terminal_sidebar {
            TerminalSidebar::Compact => "Compact".into(),
            TerminalSidebar::Expanded => "Expanded".into(),
        },
        SettingField::ResumeAgents => on_off(form.draft.resume_agents_on_restore).into(),
        SettingField::WakeMode => on_off(form.draft.wake_mode).into(),
    }
}

fn on_off(value: bool) -> &'static str {
    if value {
        "On"
    } else {
        "Off"
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HintView {
    key: String,
    suffix: String,
}

impl HintView {
    // ^ Pass the full action word; this formatter removes the mnemonic instead of duplicating it.
    fn mnemonic(key: char, action: &'static str) -> Self {
        let mut chars = action.char_indices();
        let first = chars.next().map(|(_, value)| value);
        let suffix = first
            .filter(|value| value.eq_ignore_ascii_case(&key))
            .map(|value| action[value.len_utf8()..].to_string())
            .unwrap_or_else(|| format!(" {action}"));
        Self {
            key: format!("({key})"),
            suffix,
        }
    }

    fn grouped(keys: &'static str, action: &'static str) -> Self {
        Self {
            key: format!("({keys})"),
            suffix: action.to_string(),
        }
    }

    fn named(key: &'static str, action: &'static str) -> Self {
        Self {
            key: key.to_string(),
            suffix: format!(" {action}"),
        }
    }

    fn text(&self) -> String {
        format!("{}{}", self.key, self.suffix)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SettingRowView {
    marker: String,
    label: String,
    value: Option<String>,
    gap: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SettingsTabView {
    label: &'static str,
    selected: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SettingsViewModel {
    title: String,
    tabs: Vec<SettingsTabView>,
    compact_tabs: bool,
    category: SettingsCategory,
    rows: Vec<SettingRowView>,
    selected: usize,
    stacked: bool,
    description_title: &'static str,
    description: &'static str,
    hints: Vec<HintView>,
    description_height: u16,
    hint_height: u16,
    desired_height: u16,
}

fn settings_view(form: &GlobalSettingsForm, width: u16) -> SettingsViewModel {
    let field = form.selected_field();
    let (rows, selected) = match form.editor.as_ref() {
        Some(FieldEditor::Choice(editor)) => (
            editor
                .labels
                .iter()
                .enumerate()
                .map(|(index, label)| SettingRowView {
                    marker: if index == editor.selected {
                        "●"
                    } else {
                        "○"
                    }
                    .into(),
                    label: (*label).into(),
                    value: None,
                    gap: 1,
                })
                .collect(),
            editor.selected,
        ),
        Some(FieldEditor::List(editor)) if editor.text.is_none() => {
            if editor.values.is_empty() {
                (
                    vec![SettingRowView {
                        marker: "·".into(),
                        label: "No excluded paths; press a to add".into(),
                        value: None,
                        gap: 1,
                    }],
                    0,
                )
            } else {
                (
                    editor
                        .values
                        .iter()
                        .enumerate()
                        .map(|(index, value)| SettingRowView {
                            marker: if editor.marked.contains(&index) {
                                "[x]"
                            } else {
                                "[ ]"
                            }
                            .into(),
                            label: value.clone(),
                            value: None,
                            gap: 1,
                        })
                        .collect(),
                    editor.selected,
                )
            }
        }
        _ => (
            SettingField::for_category(form.category)
                .iter()
                .map(|field| SettingRowView {
                    marker: "·".into(),
                    label: field.label().into(),
                    value: Some(setting_value(form, *field)),
                    gap: 1,
                })
                .collect(),
            form.field,
        ),
    };

    let hints = if form.accepts_text() {
        vec![
            HintView::named("←/→", "cursor"),
            HintView::named("Enter", "apply"),
            HintView::named("Esc", "revert"),
        ]
    } else if let Some(FieldEditor::List(editor)) = form.editor.as_ref() {
        let mut hints = vec![HintView::grouped("j/k", "select")];
        if !editor.values.is_empty() {
            hints.extend([
                HintView::named("Space", "mark"),
                HintView::mnemonic('e', "edit"),
                HintView::mnemonic('d', "delete"),
            ]);
        }
        hints.extend([
            HintView::mnemonic('a', "add"),
            HintView::named("Enter", "apply"),
            HintView::named("Esc", "revert"),
        ]);
        hints
    } else if form.is_editing() {
        vec![
            HintView::grouped("j/k", "select"),
            HintView::named("Enter", "apply"),
            HintView::named("Esc", "revert"),
        ]
    } else {
        let mut hints = vec![
            HintView::grouped("j/k", "select"),
            HintView::grouped("h/l", "section"),
            HintView::named("Enter", "edit"),
        ];
        if matches!(
            field,
            SettingField::ShowRelease | SettingField::ResumeAgents | SettingField::WakeMode
        ) {
            hints.push(HintView::named("Space", "toggle"));
        }
        hints.push(HintView::mnemonic('s', "save"));
        if !form.is_dirty() {
            hints.push(HintView::mnemonic('e', "edit raw"));
        }
        hints.push(HintView::named("Esc", "close"));
        hints
    };
    let stacked = width < 42;
    let mut rows = rows;
    for row in &mut rows {
        let available = usize::from(width).saturating_sub(5);
        if let Some(value) = row.value.as_mut() {
            if stacked {
                row.label = truncate_cells(&row.label, available);
                *value = truncate_cells(value, usize::from(width).saturating_sub(3));
            } else {
                *value = truncate_cells(value, available.saturating_div(2).max(1));
                let value_width = Line::from(value.as_str()).width();
                row.label = truncate_cells(&row.label, available.saturating_sub(value_width + 1));
                row.gap = available
                    .saturating_sub(Line::from(row.label.as_str()).width() + value_width)
                    .max(1);
            }
        } else {
            row.label = truncate_cells(&row.label, available);
        }
    }
    let tabs = SettingsCategory::ALL
        .iter()
        .map(|category| SettingsTabView {
            label: category.label(),
            selected: *category == form.category,
        })
        .collect::<Vec<_>>();
    let full_tab_width = tabs
        .iter()
        .map(|tab| Line::from(format!(" {} ", tab.label)).width())
        .sum::<usize>()
        + tabs.len().saturating_sub(1);
    let item_position = match form.editor.as_ref() {
        Some(FieldEditor::List(editor)) if !editor.values.is_empty() => {
            format!(" · item {}/{}", editor.selected + 1, editor.values.len())
        }
        _ => String::new(),
    };
    let body_rows = rows
        .iter()
        .map(|row| u16::from(stacked && row.value.is_some()) + 1)
        .sum::<u16>()
        .clamp(3, 8);
    let description_height = if stacked { 4 } else { 2 };
    let hint_cells = hints
        .iter()
        .map(|hint| Line::from(hint.text()).width() + 2)
        .sum::<usize>();
    let hint_width = usize::from(width).max(1);
    let hint_height = u16::try_from(hint_cells.div_ceil(hint_width))
        .unwrap_or(u16::MAX)
        .clamp(1, 4);
    SettingsViewModel {
        title: format!(
            "settings · {}/{}{}",
            form.category.index() + 1,
            SettingsCategory::ALL.len(),
            item_position
        ),
        tabs,
        compact_tabs: full_tab_width > usize::from(width),
        category: form.category,
        rows,
        selected,
        stacked,
        description_title: field.label(),
        description: field.description(),
        hints,
        description_height,
        hint_height,
        desired_height: (7 + body_rows + description_height + hint_height).clamp(15, 25),
    }
}

pub fn render(frame: &mut Frame, area: Rect, form: &GlobalSettingsForm) {
    if area.width < 3 || area.height < 3 {
        return;
    }
    let width = area.width.saturating_sub(2).min(88);
    let view = settings_view(form, width.saturating_sub(2));
    render_view(frame, area, &view, width);
}

fn render_view(frame: &mut Frame, area: Rect, view: &SettingsViewModel, width: u16) {
    let surface = popup_center(
        area,
        width,
        area.height.saturating_sub(2).min(view.desired_height),
    );
    frame.render_widget(Clear, surface);
    let block = popup_block(
        Line::default(),
        Line::default(),
        Style::default().fg(theme::ACCENT),
    );
    let inner = block.inner(surface);
    frame.render_widget(block, surface);
    if inner.width == 0 || inner.height < 3 {
        return;
    }

    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(view.description_height),
        Constraint::Length(view.hint_height),
    ])
    .split(inner);
    frame.render_widget(
        Paragraph::new(Line::styled(
            format!(" {}", view.title),
            Style::default().fg(theme::TEXT).bold(),
        )),
        rows[0],
    );
    render_section_tabs(frame, rows[1], view);
    frame.render_widget(
        Paragraph::new("─".repeat(usize::from(inner.width)))
            .style(Style::default().fg(theme::DIVIDER)),
        rows[2],
    );
    render_setting_rows(frame, rows[3], view);
    frame.render_widget(
        Paragraph::new(vec![
            Line::styled(
                view.description_title,
                Style::default().fg(theme::TEXT).bold(),
            ),
            Line::styled(view.description, Style::default().fg(theme::TEXT_MUTED)),
        ]),
        rows[4],
    );
    render_hints(frame, rows[5], &view.hints);
}

fn render_section_tabs(frame: &mut Frame, area: Rect, view: &SettingsViewModel) {
    let line = if !view.compact_tabs {
        Line::from(
            view.tabs
                .iter()
                .enumerate()
                .flat_map(|(index, tab)| {
                    let style = if tab.selected {
                        theme::group_chip(true).bold()
                    } else {
                        Style::default().fg(theme::TEXT_MUTED)
                    };
                    [
                        Some(Span::styled(format!(" {} ", tab.label), style)),
                        (index + 1 < view.tabs.len()).then_some(Span::raw(" ")),
                    ]
                    .into_iter()
                    .flatten()
                })
                .collect::<Vec<_>>(),
        )
    } else {
        Line::from(vec![
            Span::styled(" ‹ ", Style::default().fg(theme::TEXT_MUTED)),
            Span::styled(
                format!(" {} ", view.category.label()),
                theme::group_chip(true).bold(),
            ),
            Span::styled(" › ", Style::default().fg(theme::TEXT_MUTED)),
        ])
    };
    frame.render_widget(Paragraph::new(line), area);
}

fn render_setting_rows(frame: &mut Frame, area: Rect, view: &SettingsViewModel) {
    let items = view.rows.iter().map(|row| {
        let marker = Span::styled(
            format!(" {} ", row.marker),
            Style::default().fg(theme::ACCENT),
        );
        if let Some(value) = &row.value {
            if view.stacked {
                ListItem::new(vec![
                    Line::from(vec![
                        marker,
                        Span::styled(row.label.clone(), Style::default().fg(theme::TEXT)),
                    ]),
                    Line::from(Span::styled(
                        format!("   {value}"),
                        Style::default().fg(theme::ACCENT),
                    )),
                ])
            } else {
                ListItem::new(Line::from(vec![
                    marker,
                    Span::styled(row.label.clone(), Style::default().fg(theme::TEXT)),
                    Span::raw(" ".repeat(row.gap)),
                    Span::styled(value.clone(), Style::default().fg(theme::ACCENT)),
                ]))
            }
        } else {
            ListItem::new(Line::from(vec![
                marker,
                Span::styled(row.label.clone(), Style::default().fg(theme::TEXT)),
            ]))
        }
    });
    let mut state = ListState::default().with_selected(Some(view.selected));
    frame.render_stateful_widget(
        List::new(items)
            .highlight_style(theme::selected_row(false))
            .highlight_symbol("▌"),
        area,
        &mut state,
    );
}

fn render_hints(frame: &mut Frame, area: Rect, hints: &[HintView]) {
    let spans = hints
        .iter()
        .enumerate()
        .flat_map(|(index, hint)| {
            [
                (index > 0).then_some(Span::raw("  ")),
                Some(Span::styled(
                    hint.key.clone(),
                    Style::default().fg(theme::TEXT_SUBTLE),
                )),
                Some(Span::styled(
                    hint.suffix.clone(),
                    Style::default().fg(theme::TEXT_MUTED),
                )),
            ]
            .into_iter()
            .flatten()
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(Line::from(spans)).wrap(Wrap { trim: true }),
        area,
    );
}

fn truncate_cells(value: &str, max_width: usize) -> String {
    if Line::from(value).width() <= max_width {
        return value.to_string();
    }
    if max_width == 0 {
        return String::new();
    }
    let mut truncated = value.to_string();
    while !truncated.is_empty() && Line::from(format!("{truncated}…")).width() > max_width {
        truncated.pop();
    }
    if truncated.is_empty() {
        "…".into()
    } else {
        format!("{truncated}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    fn rendered(width: u16, height: u16, form: &GlobalSettingsForm) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), form))
            .unwrap();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn mnemonic_hints_derive_suffixes_instead_of_repeating_the_key() {
        assert_eq!(HintView::mnemonic('s', "save").text(), "(s)ave");
        assert_eq!(HintView::mnemonic('a', "add").text(), "(a)dd");
        assert_eq!(HintView::mnemonic('d', "delete").text(), "(d)elete");
        assert_eq!(HintView::mnemonic('e', "edit raw").text(), "(e)dit raw");
    }

    #[test]
    fn projected_hints_follow_field_and_dirty_capabilities() {
        let mut form = GlobalSettingsForm::new(GlobalConfig::default());
        let hint_text = |form: &GlobalSettingsForm| {
            settings_view(form, 74)
                .hints
                .iter()
                .map(HintView::text)
                .collect::<Vec<_>>()
        };
        let workspace = hint_text(&form);
        assert!(workspace.contains(&"(s)ave".into()));
        assert!(workspace.contains(&"(e)dit raw".into()));
        assert!(!workspace.contains(&"Space toggle".into()));

        form.next_category(false);
        let release = hint_text(&form);
        assert!(release.contains(&"Space toggle".into()));
        form.toggle();
        assert!(!hint_text(&form).contains(&"(e)dit raw".into()));
    }

    #[test]
    fn structured_terminal_binding_preserves_wire_format_and_reserves_quit() {
        let parsed = escape_binding("control+shift+a z").unwrap();
        assert_eq!(MODIFIER_LABELS[parsed.modifier], "Ctrl+Shift");
        assert_eq!(parsed.prefix_key, "a");
        assert_eq!(parsed.workspace_key, "z");
        assert!(escape_binding("a z").is_none());
        assert!(escape_binding("ctrl+a q").is_none());

        let mut form = GlobalSettingsForm::new(GlobalConfig::default());
        form.category = SettingsCategory::Terminal;
        form.field = 0;
        form.begin_or_commit().unwrap();
        form.next_field(false);
        form.begin_or_commit().unwrap();
        assert_eq!(form.draft.terminal_escape_chord, "alt+a w");

        form.next_field(false);
        form.begin_or_commit().unwrap();
        form.backspace();
        form.insert('g');
        form.begin_or_commit().unwrap();
        assert_eq!(form.draft.terminal_escape_chord, "alt+g w");

        form.next_field(false);
        form.begin_or_commit().unwrap();
        form.backspace();
        form.insert('q');
        assert!(form.begin_or_commit().is_err());
        assert!(form.is_editing());
        assert_eq!(form.draft.terminal_escape_chord, "alt+g w");
        form.backspace();
        form.insert('z');
        form.begin_or_commit().unwrap();
        assert_eq!(form.draft.terminal_escape_chord, "alt+g z");
    }

    #[test]
    fn terminal_sidebar_choice_defaults_compact_and_commits_expanded() {
        let mut form = GlobalSettingsForm::new(GlobalConfig::default());
        form.category = SettingsCategory::Terminal;
        form.field = 3;
        assert_eq!(
            setting_value(&form, SettingField::TerminalSidebar),
            "Compact"
        );

        form.begin_or_commit().unwrap();
        form.next_field(false);
        form.begin_or_commit().unwrap();

        assert_eq!(form.draft.terminal_sidebar, TerminalSidebar::Expanded);
        assert_eq!(
            setting_value(&form, SettingField::TerminalSidebar),
            "Expanded"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn wake_mode_defaults_on_and_can_be_disabled() {
        let mut form = GlobalSettingsForm::new(GlobalConfig::default());
        form.category = SettingsCategory::Runtime;
        form.field = 1;
        assert_eq!(setting_value(&form, SettingField::WakeMode), "On");

        form.toggle();

        assert!(!form.draft.wake_mode);
        assert_eq!(setting_value(&form, SettingField::WakeMode), "Off");
    }

    #[test]
    fn mixed_width_truncation_reserves_an_explicit_marker() {
        for value in [
            "설정값입니다",
            "agent👩‍💻value",
            "e\u{301}e\u{301}e\u{301}e\u{301}e\u{301}e\u{301}e\u{301}e\u{301}",
        ] {
            let truncated = truncate_cells(value, 6);
            assert!(Line::from(truncated.as_str()).width() <= 6, "{truncated:?}");
            assert!(truncated.ends_with('…'), "{truncated:?}");
        }
    }

    #[test]
    fn herdr_style_panel_uses_horizontal_tabs_and_vertical_rows() {
        let text = rendered(110, 24, &GlobalSettingsForm::new(GlobalConfig::default()));
        let workspace_row = text
            .lines()
            .position(|line| line.contains("Workspace"))
            .unwrap();
        let view_row = text.lines().position(|line| line.contains("View")).unwrap();
        let collapse_row = text
            .lines()
            .position(|line| line.contains("Automatic collapse"))
            .unwrap();
        let paths_row = text
            .lines()
            .position(|line| line.contains("Excluded worktree paths"))
            .unwrap();

        assert_eq!(
            workspace_row, view_row,
            "sections must form a horizontal tab row"
        );
        assert_eq!(
            paths_row,
            collapse_row + 1,
            "j/k fields must form vertical rows"
        );
        let border = text.lines().find(|line| line.contains('┌')).unwrap();
        let left = border
            .chars()
            .position(|character| character == '┌')
            .unwrap();
        let right = border
            .chars()
            .position(|character| character == '┐')
            .unwrap();
        assert_eq!(
            right - left + 1,
            88,
            "panel should use the larger preferred width"
        );
        assert!(text.contains("(j/k)select"));
        assert!(text.contains("(h/l)section"));
        assert!(text.contains("(s)ave"));
        let mut list_form = GlobalSettingsForm::new(GlobalConfig::default());
        list_form.next_field(false);
        list_form.begin_or_commit().unwrap();
        let list_text = rendered(110, 24, &list_form);
        assert!(list_text.contains("(a)dd"));
        assert!(list_text.contains("(d)elete"));
    }

    #[test]
    fn narrow_tabs_compact_and_tiny_views_render_without_panicking() {
        let form = GlobalSettingsForm::new(GlobalConfig::default());
        assert!(rendered(52, 18, &form).contains("View"));
        let narrow = rendered(28, 14, &form);
        assert!(narrow.contains("‹"));
        assert!(narrow.contains("Workspace"));
        assert!(narrow.contains("›"));
        let _ = rendered(12, 4, &form);
    }

    #[test]
    fn choice_cancel_preserves_draft_and_choice_commit_changes_it() {
        let mut form = GlobalSettingsForm::new(GlobalConfig::default());
        form.next_category(false);
        form.begin_or_commit().unwrap();
        form.next_field(false);
        assert!(form.cancel_editor());
        assert!(form.draft.show_release_status);
        form.begin_or_commit().unwrap();
        form.next_field(false);
        form.begin_or_commit().unwrap();
        assert!(!form.draft.show_release_status);
    }

    #[test]
    fn multi_list_supports_marked_deletion_and_cancel() {
        let config = GlobalConfig {
            exclude_worktree_paths: vec!["one".into(), "two".into(), "three".into()],
            ..GlobalConfig::default()
        };
        let mut form = GlobalSettingsForm::new(config);
        form.next_field(false);
        form.begin_or_commit().unwrap();
        form.toggle();
        form.next_field(false);
        form.toggle();
        form.delete_list_items();
        assert!(form.cancel_editor());
        assert_eq!(form.draft.exclude_worktree_paths.len(), 3);
        form.begin_or_commit().unwrap();
        form.toggle();
        form.delete_list_items();
        form.begin_or_commit().unwrap();
        assert_eq!(form.draft.exclude_worktree_paths, ["two", "three"]);
    }
}
