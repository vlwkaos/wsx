use std::collections::HashMap;

use wsx_core::runtime::{PaneId, Snapshot, TerminalFrame, TerminalId, TerminalUpdate};

// ^ [[wsx Architecture]] Workspace snapshots own metadata. This projection owns
// only accepted terminal presentation for the current daemon epoch and identities.
#[derive(Debug, Default)]
pub(crate) struct TerminalSurfaces {
    epoch: Option<u64>,
    identities: HashMap<PaneId, TerminalId>,
    frames: HashMap<PaneId, TerminalFrame>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SurfaceUpdate {
    Applied,
    Resync,
    Ignored,
}

impl TerminalSurfaces {
    pub(crate) fn reset(&mut self) {
        self.epoch = None;
        self.identities.clear();
        self.frames.clear();
    }

    pub(crate) fn reconcile(&mut self, snapshot: &Snapshot) {
        if self.epoch != Some(snapshot.epoch) {
            self.frames.clear();
        }
        self.epoch = Some(snapshot.epoch);
        self.identities = snapshot
            .panes
            .iter()
            .filter(|pane| !pane.exited)
            .map(|pane| (pane.id, pane.terminal_id))
            .collect();
        self.frames
            .retain(|pane_id, frame| self.identities.get(pane_id) == Some(&frame.terminal_id));
    }

    pub(crate) fn activate_stream(&mut self, epoch: u64, pane_id: PaneId, terminal_id: TerminalId) {
        if self.epoch != Some(epoch) {
            self.frames.clear();
            self.identities.clear();
            self.epoch = Some(epoch);
        }
        self.identities.insert(pane_id, terminal_id);
        self.frames
            .retain(|candidate, frame| self.identities.get(candidate) == Some(&frame.terminal_id));
    }

    pub(crate) fn install_full(&mut self, epoch: u64, frame: TerminalFrame) -> SurfaceUpdate {
        if !self.matches(epoch, frame.pane_id, frame.terminal_id) {
            return SurfaceUpdate::Ignored;
        }
        if self
            .frames
            .get(&frame.pane_id)
            .is_some_and(|current| current.revision >= frame.revision)
        {
            return SurfaceUpdate::Ignored;
        }
        let pane_id = frame.pane_id;
        let mut validated = None;
        if TerminalUpdate::Full(frame)
            .apply_to(&mut validated)
            .is_err()
        {
            return SurfaceUpdate::Resync;
        }
        self.frames
            .insert(pane_id, validated.expect("validated full frame"));
        SurfaceUpdate::Applied
    }

    pub(crate) fn apply(&mut self, epoch: u64, update: TerminalUpdate) -> SurfaceUpdate {
        let (pane_id, terminal_id) = update.identity();
        if !self.matches(epoch, pane_id, terminal_id) {
            return SurfaceUpdate::Ignored;
        }
        if let TerminalUpdate::Full(frame) = update {
            return self.install_full(epoch, frame);
        }
        if let TerminalUpdate::Patch {
            base_revision,
            revision,
            ..
        } = &update
        {
            if revision <= base_revision {
                return SurfaceUpdate::Resync;
            }
        }
        let mut candidate = self.frames.get(&pane_id).cloned();
        if update.apply_to(&mut candidate).is_err() {
            return SurfaceUpdate::Resync;
        }
        self.frames
            .insert(pane_id, candidate.expect("applied patch has a baseline"));
        SurfaceUpdate::Applied
    }

    pub(crate) fn frame(&self, pane_id: PaneId, terminal_id: TerminalId) -> Option<&TerminalFrame> {
        (self.identities.get(&pane_id) == Some(&terminal_id))
            .then(|| self.frames.get(&pane_id))
            .flatten()
    }

    pub(crate) fn clear_selection(&mut self, pane_id: PaneId, terminal_id: TerminalId) -> bool {
        if self.identities.get(&pane_id) != Some(&terminal_id) {
            return false;
        }
        let Some(frame) = self
            .frames
            .get_mut(&pane_id)
            .filter(|frame| frame.terminal_id == terminal_id && !frame.selection.is_empty())
        else {
            return false;
        };
        frame.selection.clear();
        true
    }

    pub(crate) fn contains(&self, epoch: u64, pane_id: PaneId, terminal_id: TerminalId) -> bool {
        self.matches(epoch, pane_id, terminal_id)
    }

    pub(crate) fn epoch(&self) -> Option<u64> {
        self.epoch
    }

    fn matches(&self, epoch: u64, pane_id: PaneId, terminal_id: TerminalId) -> bool {
        self.epoch == Some(epoch) && self.identities.get(&pane_id) == Some(&terminal_id)
    }
}
