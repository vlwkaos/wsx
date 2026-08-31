use serde::{ser::SerializeTuple, Deserialize, Deserializer, Serialize, Serializer};
use std::path::PathBuf;

macro_rules! id_type {
    ($name:ident) => {
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(pub u64);
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.fmt(f)
            }
        }
        impl std::str::FromStr for $name {
            type Err = std::num::ParseIntError;
            fn from_str(value: &str) -> Result<Self, Self::Err> {
                value.parse().map(Self)
            }
        }
    };
}

id_type!(ProjectId);
id_type!(WorktreeId);
id_type!(SessionId);
id_type!(PaneId);
id_type!(TerminalId);
id_type!(AgentInstanceId);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectSpec {
    pub path: PathBuf,
    pub name: String,
    pub worktrees: Vec<WorktreeSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeSpec {
    pub path: PathBuf,
    pub branch: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub id: ProjectId,
    pub path: PathBuf,
    pub name: String,
    pub revision: u64,
    #[serde(default)]
    pub last_agent_active_unix_ms: Option<u64>,
    #[serde(default)]
    pub last_terminal_active_unix_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Worktree {
    pub id: WorktreeId,
    pub project_id: ProjectId,
    pub path: PathBuf,
    pub branch: String,
    pub revision: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitAxis {
    Horizontal,
    Vertical,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PaneLayout {
    Leaf {
        pane_id: PaneId,
    },
    Split {
        axis: SplitAxis,
        ratio_millis: u16,
        first: Box<PaneLayout>,
        second: Box<PaneLayout>,
    },
}

impl PaneLayout {
    pub fn panes(&self, output: &mut Vec<PaneId>) {
        match self {
            Self::Leaf { pane_id } => output.push(*pane_id),
            Self::Split { first, second, .. } => {
                first.panes(output);
                second.panes(output);
            }
        }
    }

    pub fn split(&mut self, target: PaneId, pane_id: PaneId, axis: SplitAxis) -> bool {
        match self {
            Self::Leaf { pane_id: current } if *current == target => {
                *self = Self::Split {
                    axis,
                    ratio_millis: 500,
                    first: Box::new(Self::Leaf { pane_id: target }),
                    second: Box::new(Self::Leaf { pane_id }),
                };
                true
            }
            Self::Split { first, second, .. } => {
                first.split(target, pane_id, axis) || second.split(target, pane_id, axis)
            }
            Self::Leaf { .. } => false,
        }
    }

    pub fn remove(&mut self, target: PaneId) -> bool {
        match self {
            Self::Leaf { .. } => false,
            Self::Split { first, second, .. } => {
                if matches!(first.as_ref(), Self::Leaf { pane_id } if *pane_id == target) {
                    *self = (**second).clone();
                    true
                } else if matches!(second.as_ref(), Self::Leaf { pane_id } if *pane_id == target) {
                    *self = (**first).clone();
                    true
                } else {
                    first.remove(target) || second.remove(target)
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    pub worktree_id: WorktreeId,
    pub label: String,
    pub primary_pane: PaneId,
    pub focused_pane: PaneId,
    pub panes: Vec<PaneId>,
    pub layout: PaneLayout,
    pub revision: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    #[default]
    Unknown,
    Idle,
    Working,
    Blocked,
    Done,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AgentCapabilities {
    pub prompt: bool,
    pub resume: bool,
    pub lifecycle: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentInfo {
    pub id: AgentInstanceId,
    pub provider: String,
    pub state: AgentState,
    pub conversation_id: Option<String>,
    pub capabilities: AgentCapabilities,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pane {
    pub id: PaneId,
    pub terminal_id: TerminalId,
    pub session_id: SessionId,
    pub label: String,
    pub agent: Option<AgentInfo>,
    pub exited: bool,
    pub revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanePorts {
    pub pane_id: PaneId,
    pub tcp: Vec<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub protocol: u32,
    pub epoch: u64,
    pub revision: u64,
    pub projects: Vec<Project>,
    pub worktrees: Vec<Worktree>,
    pub sessions: Vec<Session>,
    pub panes: Vec<Pane>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub listening_ports: Vec<PanePorts>,
    #[serde(default)]
    pub capabilities: Capabilities,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Capabilities {
    pub pane_splits: bool,
    pub plugins: bool,
    pub agent_reports: bool,
    pub listening_ports: bool,
    pub process_restore: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CellModifiers {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
    pub dim: bool,
    pub strike: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CellWidth {
    #[default]
    Narrow,
    Wide,
    SpacerHead,
    SpacerTail,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Cell {
    pub symbol: String,
    pub fg: Option<[u8; 3]>,
    pub bg: Option<[u8; 3]>,
    pub modifiers: CellModifiers,
    pub width: CellWidth,
}

impl CellModifiers {
    fn bits(self) -> u8 {
        u8::from(self.bold)
            | (u8::from(self.italic) << 1)
            | (u8::from(self.underline) << 2)
            | (u8::from(self.inverse) << 3)
            | (u8::from(self.dim) << 4)
            | (u8::from(self.strike) << 5)
    }

    fn from_bits(bits: u8) -> Result<Self, &'static str> {
        if bits & !0x3f != 0 {
            return Err("terminal cell modifier bits are invalid");
        }
        Ok(Self {
            bold: bits & 1 != 0,
            italic: bits & 2 != 0,
            underline: bits & 4 != 0,
            inverse: bits & 8 != 0,
            dim: bits & 16 != 0,
            strike: bits & 32 != 0,
        })
    }
}

impl CellWidth {
    fn code(self) -> u8 {
        match self {
            Self::Narrow => 0,
            Self::Wide => 1,
            Self::SpacerHead => 2,
            Self::SpacerTail => 3,
        }
    }

    fn from_code(code: u8) -> Result<Self, &'static str> {
        match code {
            0 => Ok(Self::Narrow),
            1 => Ok(Self::Wide),
            2 => Ok(Self::SpacerHead),
            3 => Ok(Self::SpacerTail),
            _ => Err("terminal cell width code is invalid"),
        }
    }
}

impl Serialize for Cell {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut tuple = serializer.serialize_tuple(5)?;
        tuple.serialize_element(&self.symbol)?;
        tuple.serialize_element(&self.fg)?;
        tuple.serialize_element(&self.bg)?;
        tuple.serialize_element(&self.modifiers.bits())?;
        tuple.serialize_element(&self.width.code())?;
        tuple.end()
    }
}

impl<'de> Deserialize<'de> for Cell {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let (symbol, fg, bg, modifier_bits, width_code) =
            <(String, Option<[u8; 3]>, Option<[u8; 3]>, u8, u8)>::deserialize(deserializer)?;
        Ok(Self {
            symbol,
            fg,
            bg,
            modifiers: CellModifiers::from_bits(modifier_bits).map_err(serde::de::Error::custom)?,
            width: CellWidth::from_code(width_code).map_err(serde::de::Error::custom)?,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    pub x: u16,
    pub y: u16,
    pub visible: bool,
    pub blinking: bool,
    pub shape: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalFrame {
    pub pane_id: PaneId,
    pub terminal_id: TerminalId,
    pub revision: u64,
    pub cols: u16,
    pub rows: u16,
    pub cells: Vec<Cell>,
    pub cursor: Cursor,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalRowPatch {
    pub row: u16,
    pub cells: Vec<Cell>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum TerminalUpdate {
    Full(TerminalFrame),
    Patch {
        pane_id: PaneId,
        terminal_id: TerminalId,
        base_revision: u64,
        revision: u64,
        cols: u16,
        rows: u16,
        changed_rows: Vec<TerminalRowPatch>,
        cursor: Cursor,
    },
}

impl TerminalUpdate {
    pub fn identity(&self) -> (PaneId, TerminalId) {
        match self {
            Self::Full(frame) => (frame.pane_id, frame.terminal_id),
            Self::Patch {
                pane_id,
                terminal_id,
                ..
            } => (*pane_id, *terminal_id),
        }
    }

    pub fn revision(&self) -> u64 {
        match self {
            Self::Full(frame) => frame.revision,
            Self::Patch { revision, .. } => *revision,
        }
    }

    pub fn apply_to(self, frame: &mut Option<TerminalFrame>) -> Result<(), &'static str> {
        let (pane_id, terminal_id, base_revision, revision, cols, rows, changed_rows, cursor) =
            match self {
                Self::Full(full) => {
                    if full.rows == 0
                        || full.cols == 0
                        || full.cells.len() != usize::from(full.cols) * usize::from(full.rows)
                    {
                        return Err("terminal full frame dimensions are invalid");
                    }
                    *frame = Some(full);
                    return Ok(());
                }
                Self::Patch {
                    pane_id,
                    terminal_id,
                    base_revision,
                    revision,
                    cols,
                    rows,
                    changed_rows,
                    cursor,
                } => (
                    pane_id,
                    terminal_id,
                    base_revision,
                    revision,
                    cols,
                    rows,
                    changed_rows,
                    cursor,
                ),
            };
        if rows == 0 || cols == 0 || changed_rows.len() > usize::from(rows) {
            return Err("terminal patch dimensions are invalid");
        }
        let current = frame.as_mut().ok_or("terminal patch has no baseline")?;
        if current.pane_id != pane_id
            || current.terminal_id != terminal_id
            || current.revision != base_revision
            || current.cols != cols
            || current.rows != rows
            || current.cells.len() != usize::from(cols) * usize::from(rows)
        {
            return Err("terminal patch baseline does not match");
        }
        for patch in changed_rows {
            if patch.row >= rows || patch.cells.len() != usize::from(cols) {
                return Err("terminal patch row is invalid");
            }
            let start = usize::from(patch.row) * usize::from(cols);
            current.cells[start..start + usize::from(cols)].clone_from_slice(&patch.cells);
        }
        current.revision = revision;
        current.cursor = cursor;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyCode {
    Text,
    Enter,
    Backspace,
    Tab,
    Escape,
    Insert,
    Delete,
    Home,
    End,
    PageUp,
    PageDown,
    Left,
    Right,
    Up,
    Down,
    Function(u8),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyEvent {
    pub code: KeyCode,
    pub text: String,
    pub shift: bool,
    pub control: bool,
    pub alt: bool,
    pub super_key: bool,
    pub repeat: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseAction {
    Press,
    Release,
    Motion,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
    WheelLeft,
    WheelRight,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MouseEvent {
    pub action: MouseAction,
    pub button: MouseButton,
    pub x: u16,
    pub y: u16,
    pub shift: bool,
    pub control: bool,
    pub alt: bool,
    pub super_key: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginManifest {
    pub api_version: u32,
    pub id: String,
    pub name: String,
    pub command: Vec<String>,
    pub events: Vec<String>,
    pub enabled: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_project_json_without_activity_deserializes() {
        let project: Project =
            serde_json::from_str(r#"{"id":1,"path":"/repo","name":"repo","revision":2}"#).unwrap();
        assert_eq!(project.last_agent_active_unix_ms, None);
        assert_eq!(project.last_terminal_active_unix_ms, None);
    }

    #[test]
    fn split_and_remove_preserve_pane_identity() {
        let mut layout = PaneLayout::Leaf { pane_id: PaneId(1) };
        assert!(layout.split(PaneId(1), PaneId(2), SplitAxis::Vertical));
        let mut panes = Vec::new();
        layout.panes(&mut panes);
        assert_eq!(panes, vec![PaneId(1), PaneId(2)]);
        assert!(layout.remove(PaneId(1)));
        assert_eq!(layout, PaneLayout::Leaf { pane_id: PaneId(2) });
    }

    #[test]
    fn terminal_cell_compact_wire_round_trips_width_and_style() {
        let cell = Cell {
            symbol: "界".into(),
            fg: Some([1, 2, 3]),
            bg: Some([4, 5, 6]),
            modifiers: CellModifiers {
                bold: true,
                underline: true,
                ..CellModifiers::default()
            },
            width: CellWidth::Wide,
        };
        let encoded = serde_json::to_string(&cell).unwrap();
        assert_eq!(encoded, r#"["界",[1,2,3],[4,5,6],5,1]"#);
        assert_eq!(serde_json::from_str::<Cell>(&encoded).unwrap(), cell);
        assert!(serde_json::from_str::<Cell>(r#"["x",null,null,64,0]"#).is_err());
        assert!(serde_json::from_str::<Cell>(r#"["x",null,null,0,4]"#).is_err());
    }

    #[test]
    fn terminal_full_frame_rejects_invalid_cell_count() {
        let mut baseline = None;
        assert!(TerminalUpdate::Full(TerminalFrame {
            pane_id: PaneId(1),
            terminal_id: TerminalId(2),
            revision: 1,
            cols: 2,
            rows: 2,
            cells: vec![Cell::default(); 3],
            cursor: Cursor {
                x: 0,
                y: 0,
                visible: false,
                blinking: false,
                shape: 0,
            },
        })
        .apply_to(&mut baseline)
        .is_err());
        assert!(baseline.is_none());
    }

    #[test]
    fn terminal_patch_requires_and_updates_the_exact_baseline() {
        let cursor = Cursor {
            x: 0,
            y: 0,
            visible: true,
            blinking: false,
            shape: 0,
        };
        let mut frame = Some(TerminalFrame {
            pane_id: PaneId(1),
            terminal_id: TerminalId(2),
            revision: 3,
            cols: 2,
            rows: 2,
            cells: vec![Cell::default(); 4],
            cursor,
        });
        TerminalUpdate::Patch {
            pane_id: PaneId(1),
            terminal_id: TerminalId(2),
            base_revision: 3,
            revision: 4,
            cols: 2,
            rows: 2,
            changed_rows: vec![TerminalRowPatch {
                row: 1,
                cells: vec![
                    Cell {
                        symbol: "x".into(),
                        ..Cell::default()
                    },
                    Cell::default(),
                ],
            }],
            cursor: Cursor {
                x: 1,
                y: 1,
                ..cursor
            },
        }
        .apply_to(&mut frame)
        .unwrap();
        let frame = frame.unwrap();
        assert_eq!(frame.revision, 4);
        assert_eq!(frame.cells[2].symbol, "x");
        assert_eq!((frame.cursor.x, frame.cursor.y), (1, 1));

        let mut frame = Some(frame);
        assert!(TerminalUpdate::Patch {
            pane_id: PaneId(1),
            terminal_id: TerminalId(2),
            base_revision: 3,
            revision: 5,
            cols: 2,
            rows: 2,
            changed_rows: vec![],
            cursor,
        }
        .apply_to(&mut frame)
        .is_err());
    }
}
