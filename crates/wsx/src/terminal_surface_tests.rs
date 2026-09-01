use crate::terminal_surface::{SurfaceUpdate, TerminalSurfaces};
use wsx_core::runtime::{
    AgentCapabilities, AgentInfo, AgentState, Cell, Cursor, Pane, PaneId, PaneLayout, Session,
    SessionId, Snapshot, TerminalFrame, TerminalId, TerminalRowPatch, TerminalUpdate, WorktreeId,
    PROTOCOL_VERSION,
};

fn snapshot(epoch: u64, pane: Pane, session_revision: u64) -> Snapshot {
    Snapshot {
        protocol: PROTOCOL_VERSION,
        epoch,
        revision: session_revision,
        projects: vec![],
        worktrees: vec![],
        sessions: vec![Session {
            id: pane.session_id,
            worktree_id: WorktreeId(1),
            label: "session".into(),
            primary_pane: pane.id,
            focused_pane: pane.id,
            panes: vec![pane.id],
            layout: PaneLayout::Leaf { pane_id: pane.id },
            revision: session_revision,
        }],
        panes: vec![pane],
        listening_ports: vec![],
        capabilities: Default::default(),
    }
}

fn empty_snapshot(epoch: u64) -> Snapshot {
    Snapshot {
        protocol: PROTOCOL_VERSION,
        epoch,
        revision: 1,
        projects: vec![],
        worktrees: vec![],
        sessions: vec![],
        panes: vec![],
        listening_ports: vec![],
        capabilities: Default::default(),
    }
}

fn pane(pane_id: u64, terminal_id: u64, revision: u64, agent: Option<AgentInfo>) -> Pane {
    Pane {
        id: PaneId(pane_id),
        terminal_id: TerminalId(terminal_id),
        session_id: SessionId(1),
        label: "terminal".into(),
        agent,
        exited: false,
        revision,
    }
}

fn agent(id: u64, state: AgentState) -> AgentInfo {
    AgentInfo {
        id: wsx_core::runtime::AgentInstanceId(id),
        provider: "test-agent".into(),
        state,
        conversation_id: None,
        session_ref: None,
        capabilities: AgentCapabilities::default(),
        source: "test".into(),
    }
}

fn cursor() -> Cursor {
    Cursor {
        x: 0,
        y: 0,
        visible: false,
        blinking: false,
        shape: 0,
    }
}

fn frame(pane_id: u64, terminal_id: u64, revision: u64, first: &str) -> TerminalFrame {
    TerminalFrame {
        pane_id: PaneId(pane_id),
        terminal_id: TerminalId(terminal_id),
        revision,
        cols: 2,
        rows: 1,
        cells: vec![
            Cell {
                symbol: first.into(),
                ..Cell::default()
            },
            Cell {
                symbol: "!".into(),
                ..Cell::default()
            },
        ],
        cursor: cursor(),
    }
}

#[test]
fn full_frame_is_accepted_for_the_reconciled_identity() {
    let mut surfaces = TerminalSurfaces::default();
    surfaces.reconcile(&snapshot(7, pane(1, 2, 1, None), 1));

    let accepted = frame(1, 2, 3, "ok");
    assert!(matches!(
        surfaces.install_full(7, accepted.clone()),
        SurfaceUpdate::Applied
    ));
    assert_eq!(surfaces.frame(PaneId(1), TerminalId(2)), Some(&accepted));
}

#[test]
fn metadata_only_refresh_retains_the_visible_frame() {
    let mut surfaces = TerminalSurfaces::default();
    surfaces.reconcile(&snapshot(
        7,
        pane(1, 2, 1, Some(agent(1, AgentState::Idle))),
        1,
    ));
    let accepted = frame(1, 2, 3, "keep");
    surfaces.install_full(7, accepted.clone());

    surfaces.reconcile(&snapshot(
        7,
        pane(1, 2, 2, Some(agent(2, AgentState::Working))),
        2,
    ));

    assert_eq!(surfaces.frame(PaneId(1), TerminalId(2)), Some(&accepted));
}

#[test]
fn daemon_epoch_change_drops_frames_and_stale_epoch_is_ignored() {
    let mut surfaces = TerminalSurfaces::default();
    surfaces.reconcile(&snapshot(7, pane(1, 2, 1, None), 1));
    let old = frame(1, 2, 1, "old");
    assert!(matches!(
        surfaces.install_full(7, old.clone()),
        SurfaceUpdate::Applied
    ));

    surfaces.reconcile(&snapshot(8, pane(1, 2, 1, None), 1));
    assert_eq!(surfaces.frame(PaneId(1), TerminalId(2)), None);
    assert!(matches!(
        surfaces.install_full(7, old.clone()),
        SurfaceUpdate::Ignored
    ));
    assert!(matches!(
        surfaces.apply(7, TerminalUpdate::Full(old)),
        SurfaceUpdate::Ignored
    ));
}

#[test]
fn terminal_identity_replacement_removes_the_old_frame() {
    let mut surfaces = TerminalSurfaces::default();
    surfaces.reconcile(&snapshot(1, pane(1, 2, 1, None), 1));
    surfaces.install_full(1, frame(1, 2, 1, "stale"));

    surfaces.reconcile(&snapshot(1, pane(1, 3, 1, None), 1));

    assert_eq!(surfaces.frame(PaneId(1), TerminalId(2)), None);
    assert_eq!(surfaces.frame(PaneId(1), TerminalId(3)), None);
    assert!(matches!(
        surfaces.install_full(1, frame(1, 2, 2, "late-old")),
        SurfaceUpdate::Ignored
    ));

    let fresh = frame(1, 3, 2, "fresh");
    assert!(matches!(
        surfaces.install_full(1, fresh.clone()),
        SurfaceUpdate::Applied
    ));
    assert_eq!(surfaces.frame(PaneId(1), TerminalId(3)), Some(&fresh));
}

#[test]
fn empty_snapshot_removes_all_frames() {
    let mut surfaces = TerminalSurfaces::default();
    surfaces.reconcile(&snapshot(1, pane(1, 2, 1, None), 1));
    surfaces.install_full(1, frame(1, 2, 1, "gone"));

    surfaces.reconcile(&empty_snapshot(1));

    assert_eq!(surfaces.frame(PaneId(1), TerminalId(2)), None);
}

#[test]
fn exited_pane_removes_its_frame() {
    let mut surfaces = TerminalSurfaces::default();
    surfaces.reconcile(&snapshot(1, pane(1, 2, 1, None), 1));
    surfaces.install_full(1, frame(1, 2, 1, "exited"));

    let mut exited = pane(1, 2, 2, None);
    exited.exited = true;
    surfaces.reconcile(&snapshot(1, exited, 2));

    assert_eq!(surfaces.frame(PaneId(1), TerminalId(2)), None);
}

#[test]
fn unknown_identity_is_ignored() {
    let mut surfaces = TerminalSurfaces::default();
    surfaces.reconcile(&snapshot(1, pane(1, 2, 1, None), 1));

    assert!(matches!(
        surfaces.install_full(1, frame(9, 10, 1, "unknown")),
        SurfaceUpdate::Ignored
    ));
    assert!(matches!(
        surfaces.apply(1, TerminalUpdate::Full(frame(9, 10, 1, "unknown"))),
        SurfaceUpdate::Ignored
    ));
    assert_eq!(surfaces.frame(PaneId(9), TerminalId(10)), None);
}

#[test]
fn exact_base_patch_updates_the_current_frame() {
    let mut surfaces = TerminalSurfaces::default();
    surfaces.reconcile(&snapshot(1, pane(1, 2, 1, None), 1));
    surfaces.install_full(1, frame(1, 2, 3, "a"));

    let update = TerminalUpdate::Patch {
        pane_id: PaneId(1),
        terminal_id: TerminalId(2),
        base_revision: 3,
        revision: 4,
        cols: 2,
        rows: 1,
        changed_rows: vec![TerminalRowPatch {
            row: 0,
            cells: vec![
                Cell {
                    symbol: "b".into(),
                    ..Cell::default()
                },
                Cell {
                    symbol: "?".into(),
                    ..Cell::default()
                },
            ],
        }],
        cursor: cursor(),
    };

    assert!(matches!(surfaces.apply(1, update), SurfaceUpdate::Applied));
    let visible = surfaces.frame(PaneId(1), TerminalId(2)).unwrap();
    assert_eq!(visible.revision, 4);
    assert_eq!(
        visible
            .cells
            .iter()
            .map(|cell| cell.symbol.as_str())
            .collect::<Vec<_>>(),
        vec!["b", "?"]
    );
}

#[test]
fn apply_full_accepts_only_a_newer_current_identity_revision() {
    let mut surfaces = TerminalSurfaces::default();
    surfaces.reconcile(&snapshot(1, pane(1, 2, 1, None), 1));
    let current = frame(1, 2, 3, "current");
    surfaces.install_full(1, current);

    let newer = frame(1, 2, 4, "newer");
    assert!(matches!(
        surfaces.apply(1, TerminalUpdate::Full(newer.clone())),
        SurfaceUpdate::Applied
    ));
    assert_eq!(surfaces.frame(PaneId(1), TerminalId(2)), Some(&newer));

    assert!(matches!(
        surfaces.apply(1, TerminalUpdate::Full(frame(1, 2, 4, "equal"))),
        SurfaceUpdate::Ignored
    ));
    assert!(matches!(
        surfaces.apply(1, TerminalUpdate::Full(frame(1, 2, 3, "older"))),
        SurfaceUpdate::Ignored
    ));
    assert_eq!(surfaces.frame(PaneId(1), TerminalId(2)), Some(&newer));
}

#[test]
fn patch_without_a_baseline_requests_resynchronization() {
    let mut surfaces = TerminalSurfaces::default();
    surfaces.reconcile(&snapshot(1, pane(1, 2, 1, None), 1));

    let update = TerminalUpdate::Patch {
        pane_id: PaneId(1),
        terminal_id: TerminalId(2),
        base_revision: 0,
        revision: 1,
        cols: 2,
        rows: 1,
        changed_rows: vec![],
        cursor: cursor(),
    };
    assert!(matches!(surfaces.apply(1, update), SurfaceUpdate::Resync));
    assert_eq!(surfaces.frame(PaneId(1), TerminalId(2)), None);
}

#[test]
fn patch_with_a_mismatched_baseline_preserves_the_current_frame() {
    let mut surfaces = TerminalSurfaces::default();
    surfaces.reconcile(&snapshot(1, pane(1, 2, 1, None), 1));
    let baseline = frame(1, 2, 5, "safe");
    surfaces.install_full(1, baseline.clone());

    let update = TerminalUpdate::Patch {
        pane_id: PaneId(1),
        terminal_id: TerminalId(2),
        base_revision: 4,
        revision: 6,
        cols: 2,
        rows: 1,
        changed_rows: vec![TerminalRowPatch {
            row: 0,
            cells: vec![Cell {
                symbol: "bad-base".into(),
                ..Cell::default()
            }],
        }],
        cursor: cursor(),
    };

    assert!(matches!(surfaces.apply(1, update), SurfaceUpdate::Resync));
    assert_eq!(surfaces.frame(PaneId(1), TerminalId(2)), Some(&baseline));
}

#[test]
fn malformed_patch_row_requests_resynchronization_without_mutation() {
    let mut surfaces = TerminalSurfaces::default();
    surfaces.reconcile(&snapshot(1, pane(1, 2, 1, None), 1));
    let baseline = frame(1, 2, 5, "safe");
    surfaces.install_full(1, baseline.clone());

    let update = TerminalUpdate::Patch {
        pane_id: PaneId(1),
        terminal_id: TerminalId(2),
        base_revision: 5,
        revision: 6,
        cols: 2,
        rows: 1,
        changed_rows: vec![TerminalRowPatch {
            row: 1,
            cells: vec![
                Cell {
                    symbol: "bad-row".into(),
                    ..Cell::default()
                },
                Cell {
                    symbol: "!".into(),
                    ..Cell::default()
                },
            ],
        }],
        cursor: cursor(),
    };

    assert!(matches!(surfaces.apply(1, update), SurfaceUpdate::Resync));
    assert_eq!(surfaces.frame(PaneId(1), TerminalId(2)), Some(&baseline));
}

#[test]
fn older_full_frame_cannot_replace_the_newer_frame() {
    let mut surfaces = TerminalSurfaces::default();
    surfaces.reconcile(&snapshot(1, pane(1, 2, 1, None), 1));
    let current = frame(1, 2, 5, "new");
    surfaces.install_full(1, current.clone());

    assert!(matches!(
        surfaces.install_full(1, frame(1, 2, 4, "old")),
        SurfaceUpdate::Ignored
    ));
    assert_eq!(surfaces.frame(PaneId(1), TerminalId(2)), Some(&current));
}

#[test]
fn frame_lookup_requires_the_exact_pane_and_terminal_identity() {
    let mut surfaces = TerminalSurfaces::default();
    surfaces.reconcile(&snapshot(1, pane(1, 2, 1, None), 1));
    let accepted = frame(1, 2, 1, "visible");
    surfaces.install_full(1, accepted.clone());

    assert_eq!(surfaces.frame(PaneId(1), TerminalId(9)), None);
    assert_eq!(surfaces.frame(PaneId(9), TerminalId(2)), None);
    assert_eq!(surfaces.frame(PaneId(1), TerminalId(2)), Some(&accepted));
}

#[test]
fn malformed_current_identity_update_requests_resynchronization() {
    let mut surfaces = TerminalSurfaces::default();
    surfaces.reconcile(&snapshot(1, pane(1, 2, 1, None), 1));
    let baseline = frame(1, 2, 1, "safe");
    surfaces.install_full(1, baseline.clone());

    let invalid = TerminalFrame {
        pane_id: PaneId(1),
        terminal_id: TerminalId(2),
        revision: 2,
        cols: 0,
        rows: 1,
        cells: vec![],
        cursor: cursor(),
    };
    assert!(matches!(
        surfaces.apply(1, TerminalUpdate::Full(invalid)),
        SurfaceUpdate::Resync
    ));
    assert_eq!(surfaces.frame(PaneId(1), TerminalId(2)), Some(&baseline));
}

#[test]
fn install_full_rejects_invalid_current_identity_dimensions() {
    let mut surfaces = TerminalSurfaces::default();
    surfaces.reconcile(&snapshot(1, pane(1, 2, 1, None), 1));

    let invalid = TerminalFrame {
        pane_id: PaneId(1),
        terminal_id: TerminalId(2),
        revision: 1,
        cols: 0,
        rows: 1,
        cells: vec![],
        cursor: cursor(),
    };
    assert!(matches!(
        surfaces.install_full(1, invalid),
        SurfaceUpdate::Resync
    ));
    assert_eq!(surfaces.frame(PaneId(1), TerminalId(2)), None);
}

#[test]
fn multi_pane_reconciliation_retains_one_identity_and_removes_the_other() {
    let mut initial = snapshot(1, pane(1, 2, 1, None), 1);
    initial.sessions[0].panes.push(PaneId(2));
    initial.panes.push(pane(2, 3, 1, None));

    let mut surfaces = TerminalSurfaces::default();
    surfaces.reconcile(&initial);
    let retained = frame(1, 2, 4, "retain");
    let removed = frame(2, 3, 4, "remove");
    surfaces.install_full(1, retained.clone());
    surfaces.install_full(1, removed);

    surfaces.reconcile(&snapshot(1, pane(1, 2, 2, None), 2));

    assert_eq!(surfaces.frame(PaneId(1), TerminalId(2)), Some(&retained));
    assert_eq!(surfaces.frame(PaneId(2), TerminalId(3)), None);
}
