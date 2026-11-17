//! Where each open task of a project sits among the others: the one rule for the order of the
//! lists and for the chain a task page shows. Web and TUI draw `view::flow`'s `place`, they
//! never sort or count again. ref: docs/tasks.md "Where a task goes"

use super::needs::{self, Stage};
use super::status::{self, Holder};
use super::store::TaskDir;
use super::TaskRecord;
use anyhow::Result;
use serde::Serialize;
use std::collections::{HashMap, HashSet};

/// Who has the ball at the head of a chain: the owner, a worker or CI, or nobody who acts now.
/// The bands are in this order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Band {
    You,
    Moving,
    Waiting,
}

/// The depth below which a chain's rows stop moving in; the row still names what it waits for.
const DEPTH_SHOWN: u8 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Place {
    /// The head of the chain, or of the parent's group, this task sits in.
    pub root: u32,
    /// The card the list draws it in: its chain's root, but one card for every task that waits
    /// alone in the check queue (`Holder::Queue`), so their rows sit together under one holder.
    pub card: u32,
    /// The task it nests under: the task it waits for (`after`), else its parent.
    pub under: Option<u32>,
    pub depth: u8,
    /// The open tasks that wait for it, through any number of `after` links.
    pub holds: u32,
    /// The band of the group's head.
    pub band: Band,
    /// A child's number among its parent's children that did not close: `{at, of}`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sibling: Option<Sibling>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Sibling {
    pub at: u32,
    pub of: u32,
}

/// The places of a project's open tasks and their list order: bands, then the chain that holds
/// the most work, then the oldest; inside a chain the head first, each task before the tasks
/// that wait for it, by number.
#[derive(Debug, Default)]
pub struct Chains {
    places: HashMap<u32, Place>,
    order: Vec<u32>,
}

impl Chains {
    pub fn of(tasks: &[(TaskDir, Result<TaskRecord>)], land_auto: bool) -> Self {
        let all: HashMap<u32, (&TaskDir, &TaskRecord)> = tasks
            .iter()
            .filter_map(|(dir, record)| Some((dir, record.as_ref().ok()?)))
            .map(|(dir, record)| (record.id, (dir, record)))
            .collect();
        let open: HashSet<u32> = all
            .values()
            .filter(|(_, record)| record.state.is_open())
            .map(|(_, record)| record.id)
            .collect();
        let sibling_of = |task: u32, parent: u32| all[&task].1.parent == Some(parent);
        let mut under: HashMap<u32, Option<u32>> = open
            .iter()
            .map(|id| {
                let record = all[id].1;
                let after = record
                    .after
                    .as_ref()
                    .map(|a| a.task)
                    .filter(|t| open.contains(t));
                let parent = record.parent.filter(|p| open.contains(p));
                let found = match (after, parent) {
                    (Some(after), Some(parent)) if sibling_of(after, parent) => Some(after),
                    (_, Some(parent)) => Some(parent),
                    (after, None) => after,
                };
                (*id, found)
            })
            .collect();
        break_cycles(&mut under);
        let mut kids: HashMap<u32, Vec<u32>> = HashMap::new();
        for (id, above) in &under {
            if let Some(above) = above {
                kids.entry(*above).or_default().push(*id);
            }
        }
        kids.values_mut().for_each(|ids| ids.sort_unstable());
        let mut waiters: HashMap<u32, Vec<u32>> = HashMap::new();
        for id in &open {
            if let Some(after) = all[id].1.after.as_ref().filter(|a| open.contains(&a.task)) {
                waiters.entry(after.task).or_default().push(*id);
            }
        }
        let holds = |id: u32| {
            let (mut seen, mut stack) = (HashSet::new(), vec![id]);
            while let Some(next) = stack.pop() {
                for waiter in waiters.get(&next).into_iter().flatten() {
                    if *waiter != id && seen.insert(*waiter) {
                        stack.push(*waiter);
                    }
                }
            }
            seen.len() as u32
        };
        let band_of = |id: u32| {
            let (dir, record) = all[&id];
            if needs::stage(dir, record, land_auto) == Stage::NeedsYou {
                return Band::You;
            }
            match status::waits_on(dir, record, land_auto).map(|on| on.kind) {
                Some(Holder::Task | Holder::Manager | Holder::Queue) => Band::Waiting,
                _ => Band::Moving,
            }
        };
        // ^ A task that waits alone in the check queue: no task waits for it or nests under it.
        let queued_alone = |id: u32| {
            let (dir, record) = all[&id];
            !kids.contains_key(&id)
                && status::waits_on(dir, record, land_auto)
                    .is_some_and(|on| on.kind == Holder::Queue)
        };
        let mut roots: Vec<(Band, bool, std::cmp::Reverse<u32>, u32)> = open
            .iter()
            .filter(|id| under[id].is_none())
            .map(|id| {
                let band = band_of(*id);
                let alone = band == Band::Waiting && queued_alone(*id);
                (band, !alone, std::cmp::Reverse(holds(*id)), *id)
            })
            .collect();
        roots.sort_unstable();
        let mut chains = Self::default();
        let mut queue_card = None;
        for (band, together, _, root) in roots {
            let card = match together {
                true => root,
                false => *queue_card.get_or_insert(root),
            };
            let mut stack = vec![(root, 0u8)];
            while let Some((id, depth)) = stack.pop() {
                let record = all[&id].1;
                let sibling = record.parent.filter(|p| open.contains(p)).map(|parent| {
                    let mut among: Vec<u32> = all
                        .values()
                        .filter(|(_, other)| {
                            other.parent == Some(parent) && other.state != super::TaskState::Closed
                        })
                        .map(|(_, other)| other.id)
                        .collect();
                    among.sort_unstable();
                    Sibling {
                        at: among
                            .iter()
                            .position(|other| *other == id)
                            .map_or(0, |at| at as u32 + 1),
                        of: among.len() as u32,
                    }
                });
                chains.order.push(id);
                chains.places.insert(
                    id,
                    Place {
                        root,
                        card,
                        under: under[&id],
                        depth: depth.min(DEPTH_SHOWN),
                        holds: holds(id),
                        band,
                        sibling,
                    },
                );
                // ^ Reversed, so the lowest number is popped first.
                for kid in kids.get(&id).into_iter().flatten().rev() {
                    stack.push((*kid, depth.saturating_add(1)));
                }
            }
        }
        chains
    }

    pub fn place(&self, id: u32) -> Option<&Place> {
        self.places.get(&id)
    }

    /// The open tasks in list order.
    pub fn order(&self) -> &[u32] {
        &self.order
    }

    /// The tasks of `id`'s group in list order: upstream, the task itself, downstream.
    pub fn group_of(&self, id: u32) -> Vec<u32> {
        let Some(place) = self.place(id) else {
            return Vec::new();
        };
        self.order
            .iter()
            .copied()
            .filter(|other| self.places[other].root == place.root)
            .collect()
    }
}

/// Two tasks that wait for each other never end by themselves: the lowest number of each loop
/// becomes a head, so the loop still shows. ref: docs/tasks.md "After another task"
fn break_cycles(under: &mut HashMap<u32, Option<u32>>) {
    let mut ids: Vec<u32> = under.keys().copied().collect();
    ids.sort_unstable();
    for start in ids {
        let mut path = vec![start];
        while let Some(Some(above)) = under.get(path.last().unwrap()).copied() {
            if let Some(at) = path.iter().position(|id| *id == above) {
                let lowest = *path[at..].iter().min().unwrap();
                under.insert(lowest, None);
                break;
            }
            path.push(above);
        }
    }
}
