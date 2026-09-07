use std::collections::BTreeSet;

use ratatui::{
    prelude::*,
    widgets::{Clear, Paragraph},
};
use wsx_core::integration::{InstallStatus, IntegrationMetadata, IntegrationTarget};

use super::{popup_block, popup_center, theme};

#[derive(Debug, Clone)]
pub struct IntegrationManager {
    metadata: Vec<IntegrationMetadata>,
    selected: usize,
    marked: BTreeSet<IntegrationTarget>,
}

impl IntegrationManager {
    pub fn new(metadata: Vec<IntegrationMetadata>) -> Self {
        Self {
            metadata,
            selected: 0,
            marked: BTreeSet::new(),
        }
    }

    pub fn replace_metadata(&mut self, metadata: Vec<IntegrationMetadata>) {
        self.metadata = metadata;
        self.selected = self.selected.min(self.metadata.len().saturating_sub(1));
        self.marked.retain(|target| {
            self.metadata
                .iter()
                .any(|item| item.target == *target && installable(item))
        });
    }

    pub fn navigate(&mut self, backwards: bool) {
        if self.metadata.is_empty() {
            return;
        }
        self.selected = if backwards {
            (self.selected + self.metadata.len() - 1) % self.metadata.len()
        } else {
            (self.selected + 1) % self.metadata.len()
        };
    }

    pub fn toggle(&mut self) {
        let Some(metadata) = self.metadata.get(self.selected) else {
            return;
        };
        if !installable(metadata) {
            return;
        }
        if !self.marked.remove(&metadata.target) {
            self.marked.insert(metadata.target);
        }
    }

    pub fn targets(&self) -> Vec<IntegrationTarget> {
        self.marked.iter().copied().collect()
    }
}

fn installable(metadata: &IntegrationMetadata) -> bool {
    metadata.available && metadata.compatible && metadata.install_status != InstallStatus::Current
}

fn status(metadata: &IntegrationMetadata) -> &'static str {
    if !metadata.available {
        "Not detected"
    } else if !metadata.compatible {
        metadata.compatibility_note.unwrap_or("Unsupported CLI")
    } else {
        match metadata.install_status {
            InstallStatus::Missing => "Not installed",
            InstallStatus::Current => "Current",
            InstallStatus::Outdated => "Update available",
        }
    }
}

pub fn render(frame: &mut Frame, area: Rect, manager: &IntegrationManager) {
    let width = area.width.saturating_sub(4).min(72);
    let height = area.height.saturating_sub(2).min(22);
    let popup = popup_center(area, width, height);
    frame.render_widget(Clear, popup);
    let lines = manager
        .metadata
        .iter()
        .enumerate()
        .map(|(index, metadata)| {
            let cursor = if index == manager.selected {
                "›"
            } else {
                " "
            };
            let mark = if manager.marked.contains(&metadata.target) {
                "[x]"
            } else if installable(metadata) {
                "[ ]"
            } else {
                "   "
            };
            let style = if index == manager.selected {
                Style::default().fg(theme::ACCENT).bold()
            } else if metadata.available {
                Style::default().fg(theme::TEXT)
            } else {
                Style::default().fg(theme::TEXT_MUTED)
            };
            Line::from(format!(
                "{cursor} {mark} {:<24} {}",
                metadata.label,
                status(metadata)
            ))
            .style(style)
        })
        .collect::<Vec<_>>();
    let block = popup_block(
        Line::from(" Agent integrations "),
        Line::from(" (j/k)navigate  (Space)select  (Enter)install  (Esc)back "),
        Style::default().fg(theme::ACCENT),
    );
    frame.render_widget(Paragraph::new(lines).block(block), popup);
}

#[cfg(test)]
mod tests {
    use super::*;
    use wsx_core::integration::LifecycleCapability;

    fn metadata(
        target: IntegrationTarget,
        available: bool,
        install_status: InstallStatus,
    ) -> IntegrationMetadata {
        IntegrationMetadata {
            target,
            cli_value: target.cli_value(),
            label: target.label(),
            lifecycle: LifecycleCapability::Authoritative,
            available,
            compatible: true,
            compatibility_note: None,
            install_status,
            installed_version: None,
            expected_version: target.expected_version(),
        }
    }

    #[test]
    fn only_detected_noncurrent_integrations_can_be_selected() {
        let mut manager = IntegrationManager::new(vec![
            metadata(IntegrationTarget::Pi, false, InstallStatus::Missing),
            metadata(IntegrationTarget::Codex, true, InstallStatus::Current),
            metadata(IntegrationTarget::Claude, true, InstallStatus::Outdated),
        ]);
        manager.toggle();
        manager.navigate(false);
        manager.toggle();
        manager.navigate(false);
        manager.toggle();
        assert_eq!(manager.targets(), vec![IntegrationTarget::Claude]);
    }
}
