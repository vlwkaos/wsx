use crate::ui::{
    theme,
    workspace_nav::{group_label, render_scrollbar, SidebarLayout},
};
use ratatui::{
    prelude::*,
    widgets::{List, ListItem, ListState, Paragraph},
};
use std::path::Path;
use wsx_core::config::global::{GlobalConfig, GroupKey};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupRow {
    pub key: GroupKey,
    pub name: String,
    pub project_count: usize,
    pub checked: bool,
}

pub fn group_rows(
    config: &GlobalConfig,
    active_group: Option<&GroupKey>,
    assign_path: Option<&Path>,
) -> Vec<GroupRow> {
    let keys = if assign_path.is_some() {
        config.groups.iter().cloned().map(GroupKey::Named).collect()
    } else {
        config.ordered_group_keys()
    };
    keys.into_iter()
        .map(|key| {
            let project_count = match &key {
                GroupKey::Ungrouped => config
                    .projects
                    .iter()
                    .filter(|p| p.groups.is_empty())
                    .count(),
                GroupKey::Named(name) => config
                    .projects
                    .iter()
                    .filter(|p| p.groups.contains(name))
                    .count(),
            };
            let checked = if let Some(path) = assign_path {
                matches!(&key, GroupKey::Named(name) if config.project_groups(path).contains(name))
            } else {
                active_group == Some(&key)
            };
            GroupRow {
                name: group_label(&key).to_string(),
                key,
                project_count,
                checked,
            }
        })
        .collect()
}

pub struct GroupManagerView<'a> {
    pub selected: usize,
    pub scroll: usize,
    pub config: &'a GlobalConfig,
    pub active_group: Option<&'a GroupKey>,
    pub assign_path: Option<&'a Path>,
}

pub fn render_group_manager(frame: &mut Frame, area: Rect, view: GroupManagerView<'_>) {
    let GroupManagerView {
        selected,
        scroll,
        config,
        active_group,
        assign_path,
    } = view;
    let layout = SidebarLayout::with_header(area);
    let title = if assign_path.is_some() {
        " Assign project groups "
    } else {
        " Groups "
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            title,
            Style::default().fg(theme::TEXT).bold(),
        ))),
        layout.header,
    );
    let rows = group_rows(config, active_group, assign_path);
    let row_count = rows.len();
    let items = rows.into_iter().map(|row| {
        let marker = if row.checked { "●" } else { "○" };
        ListItem::new(Line::from(vec![
            Span::styled(
                format!(" {marker} "),
                Style::default().fg(if row.checked {
                    theme::ACCENT
                } else {
                    theme::TEXT_SUBTLE
                }),
            ),
            Span::styled(row.name, Style::default().fg(theme::TEXT)),
            Span::styled(
                format!("  {}", row.project_count),
                Style::default().fg(theme::TEXT_MUTED),
            ),
        ]))
    });
    let mut state = ListState::default().with_offset(scroll);
    if row_count > 0 {
        state.select(Some(selected.min(row_count - 1)));
    }
    let list = List::new(items)
        .style(Style::default().fg(theme::TEXT))
        .highlight_style(theme::selected_row(false))
        .highlight_symbol("▌");
    frame.render_stateful_widget(list, layout.list, &mut state);
    render_scrollbar(
        frame,
        layout.scrollbar,
        row_count,
        usize::from(layout.list.height),
        scroll,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use wsx_core::config::global::ProjectEntry;
    fn config() -> GlobalConfig {
        GlobalConfig {
            groups: vec!["work".into(), "personal".into()],
            projects: vec![ProjectEntry {
                name: "wsx".into(),
                path: PathBuf::from("/wsx"),
                groups: vec!["work".into()],
                aliases: Default::default(),
            }],
            ..Default::default()
        }
    }

    #[test]
    fn switch_has_ungrouped_then_named_rows_and_assign_has_named_only() {
        let config = config();
        let switch = group_rows(&config, None, None);
        assert_eq!(
            switch.iter().map(|r| &r.key).collect::<Vec<_>>(),
            vec![
                &GroupKey::Ungrouped,
                &GroupKey::Named("work".into()),
                &GroupKey::Named("personal".into()),
            ]
        );
        assert_eq!(switch[1].project_count, 1);

        let assign = group_rows(&config, None, Some(Path::new("/wsx")));
        assert_eq!(
            assign.iter().map(|r| &r.key).collect::<Vec<_>>(),
            vec![
                &GroupKey::Named("work".into()),
                &GroupKey::Named("personal".into()),
            ]
        );
        assert!(assign[0].checked);
        assert!(!assign[1].checked);
    }
}
