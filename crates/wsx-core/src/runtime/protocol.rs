use super::domain::*;
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::{
    cmp::Ordering,
    io,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

// ^ [[Terminal Stream Protocol v3]] Wire-version history and compatibility boundaries.
pub const PROTOCOL_VERSION: u32 = 11;
pub const MAX_REQUEST_BYTES: usize = 1024 * 1024;
pub const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;
pub const WSX_PANE_ID_ENV: &str = "WSX_PANE_ID";
pub const WSX_RUNTIME_GENERATION_ENV: &str = "WSX_RUNTIME_GENERATION";
pub const WSX_VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn compare_wsx_versions(left: &str, right: &str) -> Option<Ordering> {
    let left = parse_wsx_version(left)?;
    let right = parse_wsx_version(right)?;
    let core = left.core.cmp(&right.core);
    if core != Ordering::Equal {
        return Some(core);
    }
    match (left.prerelease, right.prerelease) {
        (None, None) => Some(Ordering::Equal),
        (None, Some(_)) => Some(Ordering::Greater),
        (Some(_), None) => Some(Ordering::Less),
        (Some(left), Some(right)) => compare_prerelease(left, right),
    }
}

pub fn binary_identity_version(identity: &str) -> Option<&str> {
    binary_identity_parts(identity).map(|(version, _)| version)
}

pub fn compare_binary_identities(left: &str, right: &str) -> Option<Ordering> {
    let (left_version, left_modified) = binary_identity_parts(left)?;
    let (right_version, right_modified) = binary_identity_parts(right)?;
    let version = compare_wsx_versions(left_version, right_version)?;
    if version != Ordering::Equal {
        return Some(version);
    }
    Some(left_modified.cmp(&right_modified))
}

fn binary_identity_parts(identity: &str) -> Option<(&str, u128)> {
    let fields = identity.split(':').collect::<Vec<_>>();
    let expected_fields = if cfg!(unix) { 5 } else { 3 };
    if fields.len() != expected_fields || parse_wsx_version(fields[0]).is_none() {
        return None;
    }
    let mut values = fields[1..]
        .iter()
        .map(|value| u128::from_str_radix(value, 16));
    let modified = values.next_back()?.ok()?;
    values
        .all(|value| value.is_ok())
        .then_some((fields[0], modified))
}

struct ParsedVersion<'a> {
    core: (u64, u64, u64),
    prerelease: Option<&'a str>,
}

fn parse_wsx_version(version: &str) -> Option<ParsedVersion<'_>> {
    let (precedence, build) = version
        .split_once('+')
        .map_or((version, None), |(precedence, build)| {
            (precedence, Some(build))
        });
    if build.is_some_and(|build| !valid_identifiers(build, false)) {
        return None;
    }
    let (core, prerelease) = precedence
        .split_once('-')
        .map_or((precedence, None), |(core, prerelease)| {
            (core, Some(prerelease))
        });
    if prerelease.is_some_and(|value| !valid_identifiers(value, true)) {
        return None;
    }
    let mut parts = core.split('.');
    let core = (
        parse_core_number(parts.next()?)?,
        parse_core_number(parts.next()?)?,
        parse_core_number(parts.next()?)?,
    );
    if parts.next().is_some() {
        return None;
    }
    Some(ParsedVersion { core, prerelease })
}

fn parse_core_number(value: &str) -> Option<u64> {
    if value.is_empty() || (value.len() > 1 && value.starts_with('0')) {
        return None;
    }
    value.parse().ok()
}

fn valid_identifiers(value: &str, reject_numeric_leading_zero: bool) -> bool {
    !value.is_empty()
        && value.split('.').all(|identifier| {
            !identifier.is_empty()
                && identifier
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                && !(reject_numeric_leading_zero
                    && identifier.len() > 1
                    && identifier.starts_with('0')
                    && identifier.bytes().all(|byte| byte.is_ascii_digit()))
        })
}

fn compare_prerelease(left: &str, right: &str) -> Option<Ordering> {
    let mut left = left.split('.');
    let mut right = right.split('.');
    loop {
        match (left.next(), right.next()) {
            (Some(left), Some(right)) => {
                let ordering = match (
                    left.bytes().all(|byte| byte.is_ascii_digit()),
                    right.bytes().all(|byte| byte.is_ascii_digit()),
                ) {
                    (true, true) => parse_core_number(left)?.cmp(&parse_core_number(right)?),
                    (true, false) => Ordering::Less,
                    (false, true) => Ordering::Greater,
                    (false, false) => left.cmp(right),
                };
                if ordering != Ordering::Equal {
                    return Some(ordering);
                }
            }
            (Some(_), None) => return Some(Ordering::Greater),
            (None, Some(_)) => return Some(Ordering::Less),
            (None, None) => return Some(Ordering::Equal),
        }
    }
}

pub fn binary_identity(path: &Path) -> io::Result<String> {
    let path = path.canonicalize()?;
    let metadata = path.metadata()?;
    let modified = metadata
        .modified()?
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    #[cfg(unix)]
    return Ok(format!(
        "{}:{:x}:{:x}:{:x}:{modified:x}",
        env!("CARGO_PKG_VERSION"),
        metadata.dev(),
        metadata.ino(),
        metadata.len()
    ));
    #[cfg(not(unix))]
    Ok(format!(
        "{}:{:x}:{modified:x}",
        env!("CARGO_PKG_VERSION"),
        metadata.len()
    ))
}

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
        #[serde(default)]
        tui: Option<TuiClientPresence>,
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
        #[serde(default)]
        runtime_generation: Option<String>,
        provider: String,
        state: AgentState,
        #[serde(default)]
        conversation_id: Option<String>,
        #[serde(default)]
        session_ref: Option<AgentSessionRef>,
        capabilities: AgentCapabilities,
    },
    AgentClear {
        pane_id: PaneId,
        runtime_generation: String,
        next_runtime_generation: String,
    },
    PluginList,
    PluginReload,
    LifecycleStatus,
    PrepareReplacement {
        target_binary_id: String,
    },
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
    Lifecycle(DaemonLifecycle),
    Replacement {
        disposition: ReplacementDisposition,
        live_runtimes: usize,
        #[serde(default)]
        daemon_version: String,
        #[serde(default)]
        target_version: String,
        #[serde(default)]
        blockers: Vec<ReplacementBlocker>,
        #[serde(default)]
        use_current_daemon: bool,
    },
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
        assert!(!capabilities.lifecycle_coordination);
    }

    #[test]
    fn wsx_versions_and_builds_have_numeric_precedence() {
        assert_eq!(
            compare_wsx_versions("0.21.0", "0.20.9"),
            Some(std::cmp::Ordering::Greater)
        );
        assert_eq!(
            compare_binary_identities("0.21.0:1:2:3:20", "0.21.0:4:5:6:10"),
            Some(std::cmp::Ordering::Greater)
        );
        assert_eq!(
            compare_wsx_versions("0.21.0-beta.2", "0.21.0-beta.11"),
            Some(std::cmp::Ordering::Less)
        );
        assert_eq!(
            compare_wsx_versions("0.21.0-rc.1", "0.21.0"),
            Some(std::cmp::Ordering::Less)
        );
        assert_eq!(
            compare_wsx_versions("0.21.0+build.2", "0.21.0+build.1"),
            Some(std::cmp::Ordering::Equal)
        );
        assert_eq!(
            compare_wsx_versions("0.21.0-alpha.beta", "0.21.0-alpha.rc"),
            Some(std::cmp::Ordering::Less)
        );
        assert_eq!(
            compare_wsx_versions("0.21.0-alpha10", "0.21.0-alpha2"),
            Some(std::cmp::Ordering::Less)
        );
        assert_eq!(compare_wsx_versions("0.21.0-01", "0.21.0"), None);
        assert_eq!(compare_wsx_versions("", "0.21.0"), None);
        assert_eq!(compare_wsx_versions("0.21.0", ""), None);
        for malformed in [
            "",
            "malformed",
            "0.21.0:1:2:3",
            "0.21.0:1:2:3:10:5",
            "0.21.0:1:2:3:not-hex",
        ] {
            assert_eq!(
                compare_binary_identities(malformed, "0.21.0:1:2:3:10"),
                None,
                "{malformed:?} must not be a binary identity"
            );
        }
    }

    #[test]
    fn legacy_lifecycle_requests_and_responses_default_version_fields() {
        let poll = serde_json::from_str::<Request>(
            r#"{"method":"poll","params":{"after_revision":7,"timeout_ms":1000}}"#,
        )
        .unwrap();
        assert!(matches!(poll, Request::Poll { tui: None, .. }));

        let replacement = serde_json::from_str::<Request>(
            r#"{"method":"prepare_replacement","params":{"target_binary_id":"0.20.0:1:2:3:4"}}"#,
        )
        .unwrap();
        assert!(matches!(replacement, Request::PrepareReplacement { .. }));

        let lifecycle = serde_json::from_str::<Response>(
            r#"{"type":"lifecycle","data":{"protocol":8,"epoch":7,"binary_id":"0.20.0:1:2:3:4","started_unix_ms":11,"phase":"replacement_pending","live_runtimes":2,"active_clients":1,"recovered_from_backup":false,"replacement_target":"0.21.0:1:2:3:4"}}"#,
        )
        .unwrap();
        assert!(matches!(
            lifecycle,
            Response::Lifecycle(DaemonLifecycle {
                binary_id,
                version,
                started_unix_ms: 11,
                active_tuis: 0,
                replacement_target,
                replacement_target_version,
                replacement_blockers,
                ..
            }) if binary_id == "0.20.0:1:2:3:4"
                && version.is_empty()
                && replacement_target.as_deref() == Some("0.21.0:1:2:3:4")
                && replacement_target_version.is_empty()
                && replacement_blockers.is_empty()
        ));

        let response = serde_json::from_str::<Response>(
            r#"{"type":"replacement","data":{"disposition":"deferred","live_runtimes":2}}"#,
        )
        .unwrap();
        assert!(matches!(
            response,
            Response::Replacement {
                daemon_version,
                target_version,
                blockers,
                use_current_daemon: false,
                ..
            } if daemon_version.is_empty() && target_version.is_empty() && blockers.is_empty()
        ));
    }

    #[test]
    fn lifecycle_control_is_additive_and_tagged() {
        let request = Request::PrepareReplacement {
            target_binary_id: "0.22.0:1:2:3:4".into(),
        };
        let encoded = serde_json::to_string(&request).unwrap();
        assert_eq!(serde_json::from_str::<Request>(&encoded).unwrap(), request);

        let response = Response::Lifecycle(DaemonLifecycle {
            protocol: PROTOCOL_VERSION,
            epoch: 7,
            binary_id: "0.21.0:1:2:3:4".into(),
            version: "0.21.0".into(),
            started_unix_ms: 11,
            phase: DaemonPhase::ReplacementPending,
            live_runtimes: 2,
            active_clients: 1,
            active_tuis: 1,
            recovered_from_backup: false,
            replacement_target: Some("0.22.0:1:2:3:4".into()),
            replacement_target_version: "0.22.0".into(),
            replacement_blockers: vec![ReplacementBlocker::WorkingAgent],
        });
        let encoded = serde_json::to_string(&response).unwrap();
        assert_eq!(
            serde_json::from_str::<Response>(&encoded).unwrap(),
            response
        );
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

        let Request::AgentReport {
            session_ref,
            runtime_generation,
            ..
        } = request
        else {
            panic!("expected agent report request");
        };
        assert_eq!(session_ref, None);
        assert_eq!(runtime_generation, None);
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
