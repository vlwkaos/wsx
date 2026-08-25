//! Long-lived Herdr subscriptions used only to invalidate wsx's snapshot projection.

use super::client;
use super::Client;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const READ_TIMEOUT: Duration = Duration::from_millis(250);
const MIN_RECONNECT_DELAY: Duration = Duration::from_millis(100);
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(3);
const MAX_EVENT_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventSignal {
    Dirty,
    Connected,
    Disconnected(String),
}

type StreamSlot = Arc<Mutex<Option<UnixStream>>>;

pub struct EventMonitor {
    stop: Arc<AtomicBool>,
    streams: Vec<StreamSlot>,
    threads: Vec<JoinHandle<()>>,
}

impl EventMonitor {
    pub fn start(
        client: Client,
        pane_ids: Vec<String>,
    ) -> Result<(Self, mpsc::Receiver<EventSignal>)> {
        let stop = Arc::new(AtomicBool::new(false));
        let (signal_tx, signal_rx) = mpsc::channel();
        let pane_ids = normalize_pane_ids(pane_ids);

        let lifecycle_stop = stop.clone();
        let lifecycle_client = client.clone();
        let lifecycle_signals = signal_tx.clone();
        let lifecycle_stream = Arc::new(Mutex::new(None));
        let lifecycle_thread_stream = lifecycle_stream.clone();
        let lifecycle = thread::Builder::new()
            .name("wsx-herdr-events".into())
            .spawn(move || {
                lifecycle_loop(
                    lifecycle_client,
                    lifecycle_stop,
                    lifecycle_signals,
                    lifecycle_thread_stream,
                )
            })
            .context("could not start Herdr lifecycle monitor")?;

        let status_stop = stop.clone();
        let status_stream = Arc::new(Mutex::new(None));
        let status_thread_stream = status_stream.clone();
        let status = match thread::Builder::new()
            .name("wsx-herdr-agent-events".into())
            .spawn(move || {
                status_loop(
                    client,
                    status_stop,
                    signal_tx,
                    pane_ids,
                    status_thread_stream,
                )
            }) {
            Ok(status) => status,
            Err(error) => {
                stop.store(true, Ordering::Release);
                shutdown_stream(&lifecycle_stream);
                let _ = lifecycle.join();
                return Err(error).context("could not start Herdr agent-status monitor");
            }
        };

        Ok((
            Self {
                stop,
                streams: vec![lifecycle_stream, status_stream],
                threads: vec![lifecycle, status],
            },
            signal_rx,
        ))
    }
}

impl Drop for EventMonitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // ^ Closing live subscriptions wakes reads before joining monitor threads.
        for slot in &self.streams {
            shutdown_stream(slot);
        }
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

fn shutdown_stream(slot: &StreamSlot) {
    if let Some(stream) = slot
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .as_ref()
    {
        let _ = stream.shutdown(Shutdown::Both);
    }
}

fn normalize_pane_ids(mut pane_ids: Vec<String>) -> Vec<String> {
    pane_ids.sort();
    pane_ids.dedup();
    pane_ids
}

fn lifecycle_loop(
    client: Client,
    stop: Arc<AtomicBool>,
    signals: mpsc::Sender<EventSignal>,
    stream: StreamSlot,
) {
    let subscriptions = vec![
        json!({"type": "workspace.created"}),
        json!({"type": "workspace.updated"}),
        json!({"type": "workspace.renamed"}),
        json!({"type": "workspace.closed"}),
        json!({"type": "tab.created"}),
        json!({"type": "tab.closed"}),
        json!({"type": "tab.renamed"}),
        json!({"type": "tab.moved"}),
        json!({"type": "pane.created"}),
        json!({"type": "pane.closed"}),
        json!({"type": "pane.updated"}),
        json!({"type": "pane.moved"}),
        json!({"type": "pane.exited"}),
        json!({"type": "pane.agent_detected"}),
    ];
    subscription_loop(
        &client,
        "wsx:lifecycle",
        &subscriptions,
        &stop,
        &signals,
        true,
        &stream,
    );
}

fn status_loop(
    client: Client,
    stop: Arc<AtomicBool>,
    signals: mpsc::Sender<EventSignal>,
    pane_ids: Vec<String>,
    stream: StreamSlot,
) {
    if pane_ids.is_empty() {
        while !stop.load(Ordering::Acquire) {
            thread::sleep(READ_TIMEOUT);
        }
        return;
    }
    let subscriptions = pane_ids
        .iter()
        .map(|pane_id| json!({"type": "pane.agent_status_changed", "pane_id": pane_id}))
        .collect::<Vec<_>>();
    subscription_loop(
        &client,
        "wsx:agent-status",
        &subscriptions,
        &stop,
        &signals,
        false,
        &stream,
    );
}

fn subscription_loop(
    client: &Client,
    id: &str,
    subscriptions: &[Value],
    stop: &AtomicBool,
    signals: &mpsc::Sender<EventSignal>,
    reports_health: bool,
    stream: &StreamSlot,
) {
    let mut delay = MIN_RECONNECT_DELAY;
    while !stop.load(Ordering::Acquire) {
        match stream_subscription(
            client,
            id,
            subscriptions,
            stop,
            signals,
            reports_health,
            stream,
        ) {
            Ok(()) => break,
            Err(error) => {
                if reports_health {
                    let _ = signals.send(EventSignal::Disconnected(error.to_string()));
                } else {
                    // A pane may disappear between snapshot and subscription setup.
                    // The lifecycle stream and reconciliation snapshot remain active.
                    let _ = signals.send(EventSignal::Dirty);
                }
                sleep_until_stopped(stop, delay);
                delay = (delay * 2).min(MAX_RECONNECT_DELAY);
            }
        }
    }
}

fn stream_subscription(
    client: &Client,
    id: &str,
    subscriptions: &[Value],
    stop: &AtomicBool,
    signals: &mpsc::Sender<EventSignal>,
    reports_health: bool,
    stream_slot: &StreamSlot,
) -> Result<()> {
    let mut stream = client::connect(client.socket_path(), READ_TIMEOUT)?;
    *stream_slot
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = Some(stream.try_clone()?);
    client::write_request(
        &mut stream,
        id,
        "events.subscribe",
        json!({"subscriptions": subscriptions}),
    )?;
    let mut reader = BufReader::new(stream);
    let mut line_bytes = Vec::new();
    let mut acknowledged = false;
    loop {
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }
        let Some(line) = read_event_line(&mut reader, &mut line_bytes)? else {
            continue;
        };
        if !acknowledged {
            validate_acknowledgement(&line, id)?;
            acknowledged = true;
            if reports_health {
                let _ = signals.send(EventSignal::Connected);
            }
            let _ = signals.send(EventSignal::Dirty);
            continue;
        }
        let value: Value = serde_json::from_str(&line)
            .map_err(|_| anyhow!("Herdr event stream returned invalid JSON"))?;
        if value.get("event").and_then(Value::as_str).is_none()
            || !value.get("data").is_some_and(Value::is_object)
        {
            bail!("Herdr event stream returned a malformed event");
        }
        if signals.send(EventSignal::Dirty).is_err() {
            return Ok(());
        }
    }
}

fn validate_acknowledgement(line: &str, id: &str) -> Result<()> {
    let value: Value = serde_json::from_str(line)
        .map_err(|_| anyhow!("Herdr subscription acknowledgement is invalid JSON"))?;
    if value.get("id").and_then(Value::as_str) != Some(id) {
        bail!("Herdr subscription acknowledgement id does not match");
    }
    if let Some(error) = value.get("error") {
        let code = error
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or("unknown_error");
        bail!("Herdr subscription failed ({code})");
    }
    if value.pointer("/result/type").and_then(Value::as_str) != Some("subscription_started") {
        bail!("Herdr subscription acknowledgement is malformed");
    }
    Ok(())
}

fn read_event_line(
    reader: &mut BufReader<UnixStream>,
    bytes: &mut Vec<u8>,
) -> Result<Option<String>> {
    if bytes.len() > MAX_EVENT_BYTES {
        bail!("Herdr event exceeds the size limit");
    }
    let remaining = MAX_EVENT_BYTES + 1 - bytes.len();
    match reader
        .by_ref()
        .take(remaining as u64)
        .read_until(b'\n', bytes)
    {
        Ok(0) if bytes.is_empty() => bail!("Herdr event stream closed"),
        Ok(0) => bail!("Herdr event stream closed with an incomplete event"),
        Ok(_) if bytes.len() > MAX_EVENT_BYTES || bytes.last() != Some(&b'\n') => {
            bail!("Herdr event exceeds the size limit or is incomplete")
        }
        Ok(_) => {
            bytes.pop();
            if bytes.last() == Some(&b'\r') {
                bytes.pop();
            }
            String::from_utf8(std::mem::take(bytes))
                .map(Some)
                .map_err(|_| anyhow!("Herdr event is not UTF-8"))
        }
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(error).context("could not read Herdr event"),
    }
}

fn sleep_until_stopped(stop: &AtomicBool, duration: Duration) {
    let mut remaining = duration;
    while !stop.load(Ordering::Acquire) && !remaining.is_zero() {
        let step = remaining.min(Duration::from_millis(50));
        thread::sleep(step);
        remaining = remaining.saturating_sub(step);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicU64;

    static SOCKET_SEQUENCE: AtomicU64 = AtomicU64::new(1);

    fn socket_path() -> PathBuf {
        let dir = std::env::current_dir()
            .unwrap()
            .join(".work/tests")
            .join(format!(
                "wsx-herdr-events-{}-{}",
                std::process::id(),
                SOCKET_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("herdr.sock")
    }

    #[test]
    fn acknowledgement_matches_live_protocol_shape() {
        validate_acknowledgement(
            r#"{"id":"wsx:lifecycle","result":{"type":"subscription_started"}}"#,
            "wsx:lifecycle",
        )
        .unwrap();
    }

    #[test]
    fn event_line_preserves_fragments_across_read_timeouts() {
        let (reader, mut writer) = UnixStream::pair().unwrap();
        reader
            .set_read_timeout(Some(Duration::from_millis(20)))
            .unwrap();
        let mut reader = BufReader::new(reader);
        let mut bytes = Vec::new();
        writer.write_all(br#"{"event":"workspace"#).unwrap();
        assert_eq!(read_event_line(&mut reader, &mut bytes).unwrap(), None);
        assert!(!bytes.is_empty());
        writer.write_all(b"\",\"data\":{}}\n").unwrap();
        assert_eq!(
            read_event_line(&mut reader, &mut bytes).unwrap(),
            Some(r#"{"event":"workspace","data":{}}"#.into())
        );
        assert!(bytes.is_empty());
    }

    #[test]
    fn stale_pane_error_is_not_an_acknowledgement() {
        assert!(validate_acknowledgement(
            r#"{"id":"wsx:stale:sub:1:probe","error":{"code":"pane_not_found","message":"gone"}}"#,
            "wsx:stale",
        )
        .is_err());
    }

    #[test]
    fn live_lifecycle_event_invalidates_and_monitor_joins() {
        let path = socket_path();
        let listener = UnixListener::bind(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let server = std::thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = String::new();
                BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut request)
                    .unwrap();
                let request: Value = serde_json::from_str(&request).unwrap();
                let id = request["id"].as_str().unwrap();
                writeln!(
                    stream,
                    "{}",
                    json!({"id": id, "result": {"type": "subscription_started"}})
                )
                .unwrap();
                if id == "wsx:lifecycle" {
                    writeln!(
                        stream,
                        "{}",
                        json!({"event": "workspace_created", "data": {"type": "workspace_created"}})
                    )
                    .unwrap();
                }
            }
        });

        let client = Client::new(path.clone()).unwrap();
        let (monitor, receiver) = EventMonitor::start(client, vec!["w1:p1".into()]).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut connected = false;
        let mut dirty = false;
        while std::time::Instant::now() < deadline && !(connected && dirty) {
            if let Ok(signal) = receiver.recv_timeout(Duration::from_millis(100)) {
                connected |= signal == EventSignal::Connected;
                dirty |= signal == EventSignal::Dirty;
            }
        }
        assert!(connected && dirty);
        drop(monitor);
        server.join().unwrap();
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn dropping_idle_acknowledged_monitor_closes_streams_before_joining() {
        let path = socket_path();
        let listener = UnixListener::bind(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let server = std::thread::spawn(move || {
            let mut handlers = Vec::new();
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                handlers.push(std::thread::spawn(move || {
                    let mut request = String::new();
                    BufReader::new(stream.try_clone().unwrap())
                        .read_line(&mut request)
                        .unwrap();
                    let request: Value = serde_json::from_str(&request).unwrap();
                    let id = request["id"].as_str().unwrap();
                    writeln!(
                        stream,
                        "{}",
                        json!({"id": id, "result": {"type": "subscription_started"}})
                    )
                    .unwrap();
                    let mut rest = Vec::new();
                    let _ = stream.read_to_end(&mut rest);
                }));
            }
            for handler in handlers {
                handler.join().unwrap();
            }
        });

        let client = Client::new(path.clone()).unwrap();
        let (monitor, receiver) = EventMonitor::start(client, vec!["w1:p1".into()]).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if receiver.recv_timeout(Duration::from_millis(100)) == Ok(EventSignal::Connected) {
                break;
            }
        }
        let started = std::time::Instant::now();
        drop(monitor);
        assert!(started.elapsed() < Duration::from_secs(1));
        server.join().unwrap();
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn pane_ids_are_sorted_and_deduplicated() {
        assert_eq!(
            normalize_pane_ids(vec!["p2".into(), "p1".into(), "p2".into()]),
            vec!["p1", "p2"]
        );
    }
}
