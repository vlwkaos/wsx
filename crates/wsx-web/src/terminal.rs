//! One browser terminal: a WebSocket bridged to one wsxd raw terminal stream.
//! Contract: docs/web-client.md "Terminal"; daemon side: docs/terminal-raw-stream.md.
//! ^ Binary frames carry PTY bytes in both directions. Text frames carry small JSON
//! control messages. The daemon stream holds the pane's one writable lease, and it
//! is dropped, with its lease, when the socket closes.
use crate::{http, App};
use axum::{
    extract::ws::{Message, WebSocket},
    http::HeaderMap,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    sync::{mpsc::RecvTimeoutError, Arc},
    time::Duration,
};
use wsx_core::runtime::{
    self, PaneId, RawTerminalStream, TerminalClientMessage, TerminalRawServerMessage,
};

/// The daemon refuses more per input message; a paste is split by the browser.
pub const MAX_INPUT_BYTES: usize = 64 * 1024;

pub struct Attach {
    pub pane_id: PaneId,
    pub client_id: u64,
    pub takeover: bool,
    /// `None`: keep the pane's size and let the browser scale its view.
    pub size: Option<(u16, u16)>,
    pub input: bool,
    /// The daemon's `raw_terminal_stream` capability. An older wsxd of the same
    /// protocol serves everything else, so only the terminal says it must be upgraded.
    pub raw_stream: bool,
}

/// The lease identity of one open page. A page that reconnects keeps it, so wsxd
/// replaces the page's own old stream without a takeover. It is bound to the paired
/// device, so one device cannot pick the identity of another controller.
pub fn client_id(device: &str, page: &str) -> u64 {
    if page.is_empty() {
        return runtime::new_client_id();
    }
    let digest = Sha256::digest(format!("{device}:{page}"));
    u64::from_be_bytes(digest[..8].try_into().expect("eight bytes"))
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Control {
    Resize { rows: u16, cols: u16 },
}

enum Frame {
    Control(Value),
    Bytes(Vec<u8>),
    /// A last control message; the socket closes after it.
    End(Value),
}

fn error_frame(code: &str, message: &str) -> Value {
    json!({ "type": "error", "code": code, "message": message })
}

fn frames(message: TerminalRawServerMessage) -> Vec<Frame> {
    match message {
        TerminalRawServerMessage::Replay(replay) => vec![
            Frame::Control(json!({
                "type": "replay", "rows": replay.rows, "cols": replay.cols,
                "truncated": replay.truncated,
            })),
            Frame::Bytes(replay.ansi),
        ],
        TerminalRawServerMessage::Output(bytes) => vec![Frame::Bytes(bytes)],
        TerminalRawServerMessage::Resized { rows, cols } => {
            vec![Frame::Control(
                json!({ "type": "resized", "rows": rows, "cols": cols }),
            )]
        }
        TerminalRawServerMessage::ClipboardWrite(bytes) => vec![Frame::Control(
            json!({ "type": "clipboard", "text": String::from_utf8_lossy(&bytes) }),
        )],
        TerminalRawServerMessage::Exited => vec![Frame::End(json!({ "type": "exited" }))],
        TerminalRawServerMessage::Error(error) => vec![Frame::End(match error.code.as_str() {
            "lease_transferred" | "lease_lost" => json!({ "type": "control_lost" }),
            code => error_frame(code, &error.message),
        })],
    }
}

/// Runs on a blocking thread for the life of the socket: it owns the daemon
/// stream, so dropping it sends Detach and releases the lease.
fn pump(
    app: &App,
    attach: &Attach,
    updates: &tokio::sync::mpsc::Sender<Frame>,
    input: tokio::sync::oneshot::Sender<std::sync::mpsc::SyncSender<TerminalClientMessage>>,
) {
    if !attach.raw_stream {
        let _ = updates.blocking_send(Frame::End(error_frame(
            "daemon_upgrade_required",
            "The terminal needs a newer wsx daemon. Upgrade wsx on the host.",
        )));
        return;
    }
    let (rows, cols) = attach.size.unwrap_or((0, 0));
    let stream = match RawTerminalStream::connect_raw(
        &app.client,
        attach.pane_id,
        attach.client_id,
        attach.takeover,
        rows,
        cols,
    ) {
        Ok(stream) => stream,
        Err(error) => {
            // The client reports a refusal as "<code>: <message>".
            let text = error.to_string();
            let (code, message) = text
                .split_once(": ")
                .unwrap_or(("daemon_unavailable", &text));
            let _ = updates.blocking_send(Frame::End(if code == "terminal_busy" {
                json!({ "type": "busy" })
            } else {
                error_frame(code, message)
            }));
            return;
        }
    };
    if input.send(stream.input()).is_err() {
        return;
    }
    while !updates.is_closed() {
        let message = match stream.recv_timeout(Duration::from_millis(200)) {
            Ok(message) => message,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        for frame in frames(message) {
            let last = matches!(frame, Frame::End(_));
            if updates.blocking_send(frame).is_err() || last {
                return;
            }
        }
    }
}

pub async fn bridge(app: Arc<App>, mut socket: WebSocket, attach: Attach, headers: HeaderMap) {
    let (updates_tx, mut updates) = tokio::sync::mpsc::channel::<Frame>(32);
    let (input_tx, input_rx) = tokio::sync::oneshot::channel();
    let allow_input = attach.input;
    let pump_app = app.clone();
    let pumping =
        tokio::task::spawn_blocking(move || pump(&pump_app, &attach, &updates_tx, input_tx));
    let mut input = None;
    let mut input_rx = Some(input_rx);
    let mut revocation = tokio::time::interval(Duration::from_secs(20));
    revocation.tick().await;
    loop {
        tokio::select! {
            sender = async { input_rx.as_mut().expect("guarded").await }, if input_rx.is_some() => {
                input_rx = None;
                input = sender.ok();
            }
            frame = updates.recv() => {
                let sent = match frame {
                    Some(Frame::Bytes(bytes)) => socket.send(Message::Binary(bytes.into())).await,
                    Some(Frame::Control(value)) => socket.send(Message::Text(value.to_string().into())).await,
                    Some(Frame::End(value)) => {
                        let _ = socket.send(Message::Text(value.to_string().into())).await;
                        break;
                    }
                    None => break,
                };
                if sent.is_err() {
                    break;
                }
            }
            incoming = socket.recv() => {
                let message = match incoming {
                    Some(Ok(Message::Binary(bytes))) if allow_input && bytes.len() <= MAX_INPUT_BYTES => {
                        TerminalClientMessage::Input(bytes.to_vec())
                    }
                    Some(Ok(Message::Text(text))) => match serde_json::from_str::<Control>(&text) {
                        Ok(Control::Resize { rows, cols }) if allow_input => {
                            TerminalClientMessage::Resize { rows, cols }
                        }
                        _ => {
                            let _ = socket.send(Message::Text(
                                error_frame("invalid_message", "this message is not allowed").to_string().into(),
                            )).await;
                            break;
                        }
                    },
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                    // Input without the capability, an oversized frame, a close, or a transport error.
                    _ => break,
                };
                // Before the daemon stream exists there is nothing to send to; a full
                // queue means the daemon is not reading, and the stream then ends itself.
                if let Some(input) = &input {
                    let _ = input.try_send(message);
                }
            }
            _ = revocation.tick() => {
                if http::blocking(|| http::authenticate(&app, &headers)).is_err() {
                    let _ = socket.send(Message::Text(json!({ "type": "revoked" }).to_string().into())).await;
                    break;
                }
            }
        }
    }
    // Closing the channel ends the pump within one poll interval.
    drop(updates);
    drop(socket);
    let _ = pumping.await;
}
