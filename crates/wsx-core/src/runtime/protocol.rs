use super::domain::*;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const PROTOCOL_VERSION: u32 = 10;
pub const MAX_REQUEST_BYTES: usize = 1024 * 1024;
pub const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum Request {
    Hello {
        protocol: u32,
    },
    Snapshot,
    Poll {
        after_revision: u64,
        timeout_ms: u64,
    },
    SynchronizeProjects {
        projects: Vec<ProjectSpec>,
    },
    SessionCreate {
        worktree_id: WorktreeId,
        label: String,
        command: Vec<String>,
        #[serde(default)]
        initial_input: Option<String>,
        rows: u16,
        cols: u16,
    },
    SessionRename {
        session_id: SessionId,
        label: String,
        expected_revision: u64,
    },
    SessionReorder {
        session_id: SessionId,
        target_session_id: SessionId,
        placement: SessionPlacement,
        expected_revision: u64,
    },
    SessionClose {
        session_id: SessionId,
        expected_revision: u64,
    },
    PaneSplit {
        session_id: SessionId,
        target: PaneId,
        axis: SplitAxis,
        label: String,
        command: Vec<String>,
        #[serde(default)]
        initial_input: Option<String>,
        rows: u16,
        cols: u16,
        expected_revision: u64,
    },
    PaneFocus {
        session_id: SessionId,
        pane_id: PaneId,
    },
    PaneClose {
        pane_id: PaneId,
        expected_revision: u64,
    },
    TerminalAcquire {
        pane_id: PaneId,
        client_id: u64,
        takeover: bool,
    },
    TerminalRelease {
        pane_id: PaneId,
        client_id: u64,
    },
    TerminalHeartbeat {
        pane_id: PaneId,
        client_id: u64,
    },
    TerminalSubscribe {
        pane_id: PaneId,
        client_id: u64,
        takeover: bool,
        rows: u16,
        cols: u16,
    },
    TerminalInput {
        pane_id: PaneId,
        client_id: u64,
        bytes: Vec<u8>,
    },
    TerminalKey {
        pane_id: PaneId,
        client_id: u64,
        key: KeyEvent,
    },
    TerminalPaste {
        pane_id: PaneId,
        client_id: u64,
        text: String,
    },
    TerminalMouse {
        pane_id: PaneId,
        client_id: u64,
        mouse: MouseEvent,
    },
    TerminalResize {
        pane_id: PaneId,
        client_id: u64,
        rows: u16,
        cols: u16,
    },
    View {
        pane_ids: Vec<PaneId>,
    },
    AgentReport {
        pane_id: PaneId,
        provider: String,
        state: AgentState,
        #[serde(default)]
        conversation_id: Option<String>,
        #[serde(default)]
        session_ref: Option<AgentSessionRef>,
        capabilities: AgentCapabilities,
    },
    PluginList,
    PluginReload,
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Event {
    Changed {
        revision: u64,
        entity: String,
        id: u64,
    },
    Exited {
        revision: u64,
        pane_id: PaneId,
    },
    ResyncRequired {
        revision: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiError {
    pub code: String,
    pub message: String,
}

impl ApiError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Response {
    Hello {
        protocol: u32,
        epoch: u64,
        #[serde(default)]
        capabilities: Capabilities,
    },
    Snapshot(Snapshot),
    View {
        snapshot: Snapshot,
        frames: Vec<TerminalFrame>,
    },
    Events {
        revision: u64,
        events: Vec<Event>,
    },
    Plugins(Vec<PluginManifest>),
    Created {
        revision: u64,
        id: u64,
    },
    Ack {
        revision: u64,
    },
    Error(ApiError),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum TerminalClientMessage {
    Key(KeyEvent),
    Paste(String),
    Mouse(MouseEvent),
    Input(Vec<u8>),
    Resize { rows: u16, cols: u16 },
    Heartbeat,
    Resync,
    Detach,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum TerminalServerMessage {
    Update(TerminalUpdate),
    ClipboardWrite(Vec<u8>),
    Error(ApiError),
    Exited,
}

pub fn encode_line<T: Serialize>(value: &T) -> Result<Vec<u8>, serde_json::Error> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    Ok(bytes)
}

pub fn default_socket_path() -> PathBuf {
    if let Some(path) = std::env::var_os("WSX_SOCKET") {
        return PathBuf::from(path);
    }
    let root = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .unwrap_or_else(std::env::temp_dir);
    root.join("wsx/wsx.sock")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_is_tagged_and_line_delimited() {
        let bytes = encode_line(&Request::Snapshot).unwrap();
        assert_eq!(bytes.last(), Some(&b'\n'));
        assert!(String::from_utf8(bytes).unwrap().contains("snapshot"));
    }

    #[test]
    fn unknown_methods_including_recent_clear_are_rejected() {
        for method in ["unknown_method", "project_recent_clear"] {
            let json = format!(r#"{{"method":"{method}","params":{{}}}}"#);
            assert!(serde_json::from_str::<Request>(&json).is_err(), "{method}");
        }
    }

    #[test]
    fn legacy_hello_defaults_capabilities_added_by_newer_protocols() {
        let response = serde_json::from_str::<Response>(
            r#"{"type":"hello","data":{"protocol":3,"epoch":1,"capabilities":{"pane_splits":true,"plugins":true,"agent_reports":true,"process_restore":false}}}"#,
        )
        .unwrap();

        let Response::Hello { capabilities, .. } = response else {
            panic!("expected hello response");
        };
        assert!(capabilities.pane_splits);
        assert!(!capabilities.agent_session_restore);
        assert!(!capabilities.resume_shell_fallback);
        assert!(!capabilities.listening_ports);
        assert!(!capabilities.foreground_jobs);
    }

    #[test]
    fn legacy_snapshot_defaults_missing_foreground_job_metadata() {
        let response = serde_json::from_str::<Response>(
            r#"{"type":"snapshot","data":{"protocol":8,"epoch":1,"revision":1,"projects":[],"worktrees":[],"sessions":[],"panes":[],"capabilities":{}}}"#,
        )
        .unwrap();

        let Response::Snapshot(snapshot) = response else {
            panic!("expected snapshot response");
        };
        assert!(snapshot.pane_activity.is_empty());
        assert!(!snapshot.capabilities.foreground_jobs);
    }

    #[test]
    fn legacy_agent_report_defaults_missing_session_reference() {
        let request = serde_json::from_str::<Request>(
            r#"{"method":"agent_report","params":{"pane_id":1,"provider":"pi","state":"idle","conversation_id":"legacy","capabilities":{}}}"#,
        )
        .unwrap();

        let Request::AgentReport { session_ref, .. } = request else {
            panic!("expected agent report request");
        };
        assert_eq!(session_ref, None);
    }

    #[test]
    fn legacy_session_create_defaults_missing_initial_input() {
        let request = serde_json::from_str::<Request>(
            r#"{"method":"session_create","params":{"worktree_id":1,"label":"legacy","command":[],"rows":24,"cols":80}}"#,
        )
        .unwrap();

        let Request::SessionCreate { initial_input, .. } = request else {
            panic!("expected session create request");
        };
        assert_eq!(initial_input, None);
    }

    #[test]
    fn clipboard_write_is_a_distinct_ephemeral_stream_message() {
        let bytes =
            encode_line(&TerminalServerMessage::ClipboardWrite(b"copied".to_vec())).unwrap();
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "{\"type\":\"clipboard_write\",\"data\":[99,111,112,105,101,100]}\n"
        );
    }

    #[test]
    fn legacy_terminal_wire_defaults_selection_and_pointer_bounds() {
        let full = serde_json::from_str::<TerminalUpdate>(
            r#"{"kind":"full","data":{"pane_id":1,"terminal_id":2,"revision":3,"cols":1,"rows":1,"cells":[["x",null,null,0,0]],"cursor":{"x":0,"y":0,"visible":false,"blinking":false,"shape":0}}}"#,
        )
        .unwrap();
        let TerminalUpdate::Full(full) = full else {
            panic!("expected full terminal update");
        };
        assert!(full.selection.is_empty());

        let patch = serde_json::from_str::<TerminalUpdate>(
            r#"{"kind":"patch","data":{"pane_id":1,"terminal_id":2,"base_revision":3,"revision":4,"cols":1,"rows":1,"changed_rows":[],"cursor":{"x":0,"y":0,"visible":false,"blinking":false,"shape":0}}}"#,
        )
        .unwrap();
        let TerminalUpdate::Patch { selection, .. } = patch else {
            panic!("expected terminal patch");
        };
        assert!(selection.is_empty());

        let mouse = serde_json::from_str::<MouseEvent>(
            r#"{"action":"release","button":"left","x":0,"y":0,"shift":false,"control":false,"alt":false,"super_key":false}"#,
        )
        .unwrap();
        assert!(mouse.in_bounds);
    }

    #[test]
    fn full_terminal_baseline_stays_within_compact_size_budget() {
        let cell = Cell {
            symbol: " ".into(),
            fg: Some([220, 220, 220]),
            bg: Some([8, 9, 11]),
            ..Cell::default()
        };
        let message = TerminalServerMessage::Update(TerminalUpdate::Full(TerminalFrame {
            pane_id: PaneId(1),
            terminal_id: TerminalId(2),
            revision: 1,
            cols: 120,
            rows: 40,
            cells: vec![cell; 120 * 40],
            cursor: Cursor {
                x: 0,
                y: 0,
                visible: true,
                blinking: false,
                shape: 0,
            },
            selection: Vec::new(),
        }));
        let bytes = encode_line(&message).unwrap();
        assert!(
            bytes.len() < 256 * 1024,
            "baseline was {} bytes",
            bytes.len()
        );
    }
}
