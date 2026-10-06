//! Search logical Workspace entries, then project hits onto visible ancestors.
//! ^ docs/terminal-context.md: searching never changes expansion or activity provenance.
use std::collections::HashMap;
use wsx_core::model::workspace::{FlatEntry, WorkspaceState};

#[derive(Default)]
pub(crate) struct SearchIndex {
    entries: Vec<(String, usize)>,
    pub counts: Vec<usize>,
    pub total: usize,
    query: String,
}

fn key(entry: &FlatEntry) -> [usize; 5] {
    match *entry {
        FlatEntry::Project { idx } => [0, idx, 0, 0, 0],
        FlatEntry::Worktree {
            project_idx,
            worktree_idx,
        } => [1, project_idx, worktree_idx, 0, 0],
        FlatEntry::Session {
            project_idx,
            worktree_idx,
            session_idx,
        } => [2, project_idx, worktree_idx, session_idx, 0],
        FlatEntry::Pane {
            project_idx,
            worktree_idx,
            session_idx,
            pane_idx,
        } => [3, project_idx, worktree_idx, session_idx, pane_idx],
        FlatEntry::RoutinesHeader { project_idx } => [4, project_idx, 0, 0, 0],
        FlatEntry::Routine {
            project_idx,
            routine_idx,
        } => [5, project_idx, routine_idx, 0, 0],
    }
}

impl SearchIndex {
    pub fn new(workspace: &WorkspaceState, visible: &[FlatEntry]) -> Self {
        let rows: HashMap<_, _> = visible
            .iter()
            .enumerate()
            .map(|(i, entry)| (key(entry), i))
            .collect();
        let mut index = Self {
            counts: vec![0; visible.len()],
            ..Self::default()
        };
        let mut add = |entry: FlatEntry, parent: usize, text: String| {
            let row = rows.get(&key(&entry)).copied().unwrap_or(parent);
            index.entries.push((text.to_lowercase(), row));
            row
        };
        // Only projects in the current group have a visible root. Hidden groups stay excluded.
        for entry in visible {
            let FlatEntry::Project { idx: pi } = *entry else {
                continue;
            };
            let p = &workspace.projects[pi];
            let project_row = add(entry.clone(), rows[&key(entry)], p.name.clone());
            for (wi, wt) in p.worktrees.iter().enumerate() {
                let worktree_row = add(
                    FlatEntry::Worktree {
                        project_idx: pi,
                        worktree_idx: wi,
                    },
                    project_row,
                    format!(
                        "{} {} {}",
                        wt.branch,
                        wt.alias.as_deref().unwrap_or(""),
                        wt.name
                    ),
                );
                for (si, session) in wt.sessions.iter().enumerate() {
                    let session_row = add(
                        FlatEntry::Session {
                            project_idx: pi,
                            worktree_idx: wi,
                            session_idx: si,
                        },
                        worktree_row,
                        session.display_name.clone(),
                    );
                    // Single-pane details are not independent Workspace rows, even when unfolded.
                    if session.panes.len() > 1 {
                        for (pane_idx, pane) in session.panes.iter().enumerate() {
                            add(
                                FlatEntry::Pane {
                                    project_idx: pi,
                                    worktree_idx: wi,
                                    session_idx: si,
                                    pane_idx,
                                },
                                session_row,
                                format!(
                                    "{} {}",
                                    pane.label,
                                    pane.agent.as_deref().unwrap_or("terminal")
                                ),
                            );
                        }
                    }
                }
            }
            if !p.routines.is_empty() {
                let header_row = add(
                    FlatEntry::RoutinesHeader { project_idx: pi },
                    project_row,
                    format!("{} routines", p.name),
                );
                for (routine_idx, view) in p.routines.iter().enumerate() {
                    let routine = &view.routine;
                    add(
                        FlatEntry::Routine {
                            project_idx: pi,
                            routine_idx,
                        },
                        header_row,
                        format!(
                            "{} {:?} {} {}",
                            routine.name,
                            routine.trigger,
                            routine.command.join(" "),
                            routine.prompt
                        ),
                    );
                }
            }
        }
        index
    }

    fn counts_for(&self, query: &str) -> Vec<usize> {
        let mut counts = vec![0; self.counts.len()];
        if !query.is_empty() {
            let query = query.to_lowercase();
            for (text, row) in &self.entries {
                if text.contains(&query) {
                    counts[*row] += 1;
                }
            }
        }
        counts
    }

    pub fn update(&mut self, query: &str) {
        self.counts = self.counts_for(query);
        self.total = self.counts.iter().sum();
        self.query = query.to_owned();
    }

    pub fn matches(&self, query: &str) -> Vec<usize> {
        let counts = if self.query == query {
            &self.counts
        } else {
            &self.counts_for(query)
        };
        counts
            .iter()
            .enumerate()
            .filter_map(|(row, count)| (*count > 0).then_some(row))
            .collect()
    }
}
