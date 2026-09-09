use serde_json::Value;
use std::{
    io::{self, BufRead, BufReader, Write},
    os::unix::process::CommandExt,
    path::Path,
    process::{Child, Command, Stdio},
    sync::{mpsc, Arc, Mutex, MutexGuard},
    thread,
};
use wsx_core::integration::conversation::ConversationLaunchPlan;

pub(crate) const MAX_RPC_RECORD_BYTES: usize = 4 * 1024 * 1024;
const MAX_RPC_COMMAND_BYTES: usize = 1024 * 1024;
const MAX_STDERR_LINE_BYTES: usize = 16 * 1024;
const COMMAND_QUEUE_CAPACITY: usize = 64;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum RpcRecord {
    Response {
        id: Option<String>,
        command: String,
        success: bool,
        data: Option<Value>,
        error: Option<String>,
    },
    ExtensionUi {
        id: String,
        method: String,
        body: Value,
    },
    Event {
        event_type: String,
        body: Value,
    },
    Diagnostic(String),
    RuntimeError(String),
    Exited,
}

pub(crate) struct ConversationProcess {
    child: Arc<Mutex<Child>>,
    input: Option<mpsc::SyncSender<Vec<u8>>>,
    writer: Option<thread::JoinHandle<()>>,
    output: Option<thread::JoinHandle<()>>,
    errors: Option<thread::JoinHandle<()>>,
}

impl ConversationProcess {
    pub(crate) fn spawn(
        plan: &ConversationLaunchPlan,
        cwd: &Path,
        on_record: Arc<dyn Fn(RpcRecord) + Send + Sync>,
    ) -> io::Result<Self> {
        let (program, arguments) = plan.argv.split_first().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "empty conversation argv")
        })?;
        let mut child = Command::new(program)
            .args(arguments)
            .current_dir(cwd)
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let Some(input) = child.stdin.take() else {
            stop_child(&mut child);
            return Err(io::Error::other("conversation stdin is unavailable"));
        };
        let Some(output) = child.stdout.take() else {
            stop_child(&mut child);
            return Err(io::Error::other("conversation stdout is unavailable"));
        };
        let Some(errors) = child.stderr.take() else {
            stop_child(&mut child);
            return Err(io::Error::other("conversation stderr is unavailable"));
        };
        let child = Arc::new(Mutex::new(child));
        let (input_tx, input_rx) = mpsc::sync_channel::<Vec<u8>>(COMMAND_QUEUE_CAPACITY);

        let writer_callback = Arc::clone(&on_record);
        let writer_child = Arc::clone(&child);
        let writer = match thread::Builder::new()
            .name("wsxd-conversation-input".into())
            .spawn(move || {
                let mut input = input;
                while let Ok(bytes) = input_rx.recv() {
                    if let Err(error) = input.write_all(&bytes).and_then(|_| input.flush()) {
                        writer_callback(RpcRecord::RuntimeError(error.to_string()));
                        stop_and_reap(&writer_child);
                        break;
                    }
                }
            }) {
            Ok(handle) => handle,
            Err(error) => {
                stop_and_reap(&child);
                return Err(error);
            }
        };

        let output_callback = Arc::clone(&on_record);
        let output_child = Arc::clone(&child);
        let output = match thread::Builder::new()
            .name("wsxd-conversation-output".into())
            .spawn(move || {
                let mut reader = BufReader::with_capacity(64 * 1024, output);
                loop {
                    match read_record(&mut reader) {
                        Ok(Some(record)) => output_callback(record),
                        Ok(None) => break,
                        Err(error) => {
                            output_callback(RpcRecord::RuntimeError(error.to_string()));
                            break;
                        }
                    }
                }
                stop_and_reap(&output_child);
                output_callback(RpcRecord::Exited);
            }) {
            Ok(handle) => handle,
            Err(error) => {
                drop(input_tx);
                stop_and_reap(&child);
                let _ = writer.join();
                return Err(error);
            }
        };

        let error_callback = on_record;
        let error_child = Arc::clone(&child);
        let errors = match thread::Builder::new()
            .name("wsxd-conversation-errors".into())
            .spawn(move || {
                let mut reader = BufReader::new(errors);
                loop {
                    match read_stderr_line(&mut reader) {
                        Ok(Some(line)) => error_callback(RpcRecord::Diagnostic(line)),
                        Ok(None) => break,
                        Err(error) => {
                            error_callback(RpcRecord::RuntimeError(error.to_string()));
                            stop_and_reap(&error_child);
                            break;
                        }
                    }
                }
            }) {
            Ok(handle) => handle,
            Err(error) => {
                drop(input_tx);
                stop_and_reap(&child);
                let _ = writer.join();
                let _ = output.join();
                return Err(error);
            }
        };

        Ok(Self {
            child,
            input: Some(input_tx),
            writer: Some(writer),
            output: Some(output),
            errors: Some(errors),
        })
    }

    pub(crate) fn send(&self, value: &Value) -> io::Result<()> {
        let mut bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
        if bytes.len() >= MAX_RPC_COMMAND_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Pi RPC command exceeds limit",
            ));
        }
        bytes.push(b'\n');
        self.input
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "conversation stopped"))?
            .try_send(bytes)
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "conversation command queue is full",
                ),
                mpsc::TrySendError::Disconnected(_) => {
                    io::Error::new(io::ErrorKind::BrokenPipe, "conversation stopped")
                }
            })
    }

    pub(crate) fn terminate(&mut self) {
        self.input.take();
        stop_and_reap(&self.child);
        for handle in [&mut self.writer, &mut self.output, &mut self.errors] {
            if let Some(handle) = handle.take() {
                let _ = handle.join();
            }
        }
    }
}

impl Drop for ConversationProcess {
    fn drop(&mut self) {
        self.terminate();
    }
}

fn stop_and_reap(child: &Mutex<Child>) {
    stop_child(&mut lock(child));
}

fn stop_child(child: &mut Child) {
    if matches!(child.try_wait(), Ok(Some(_))) {
        return;
    }
    let process_group = child.id() as libc::pid_t;
    unsafe {
        libc::killpg(process_group, libc::SIGKILL);
    }
    let _ = child.wait();
}

pub(crate) fn read_record(reader: &mut impl BufRead) -> io::Result<Option<RpcRecord>> {
    let mut bytes = Vec::with_capacity(4096);
    let mut bounded = std::io::Read::take(reader, (MAX_RPC_RECORD_BYTES + 1) as u64);
    let read = bounded.read_until(b'\n', &mut bytes)?;
    if read == 0 {
        return Ok(None);
    }
    if bytes.len() > MAX_RPC_RECORD_BYTES || bytes.last() != Some(&b'\n') {
        return Err(invalid("Pi RPC record is incomplete or exceeds limit"));
    }
    bytes.pop();
    if bytes.last() == Some(&b'\r') {
        bytes.pop();
    }
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    classify(value).map(Some)
}

fn read_stderr_line(reader: &mut impl BufRead) -> io::Result<Option<String>> {
    let mut bytes = Vec::new();
    let mut bounded = std::io::Read::take(reader, (MAX_STDERR_LINE_BYTES + 1) as u64);
    let read = bounded.read_until(b'\n', &mut bytes)?;
    if read == 0 {
        return Ok(None);
    }
    if bytes.len() > MAX_STDERR_LINE_BYTES || bytes.last() != Some(&b'\n') {
        return Err(invalid(
            "conversation stderr line is incomplete or exceeds limit",
        ));
    }
    bytes.pop();
    if bytes.last() == Some(&b'\r') {
        bytes.pop();
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|error| invalid(error.to_string()))
}

fn classify(value: Value) -> io::Result<RpcRecord> {
    let Some(object) = value.as_object() else {
        return Err(invalid("Pi RPC record must be an object"));
    };
    let record_type = bounded_string(object.get("type"), "type")?;
    match record_type.as_str() {
        "response" => Ok(RpcRecord::Response {
            id: optional_bounded_string(object.get("id"), "id")?,
            command: bounded_string(object.get("command"), "command")?,
            success: object
                .get("success")
                .and_then(Value::as_bool)
                .ok_or_else(|| invalid("Pi RPC response success must be a boolean"))?,
            data: object.get("data").cloned(),
            error: optional_bounded_string(object.get("error"), "error")?,
        }),
        "extension_ui_request" => Ok(RpcRecord::ExtensionUi {
            id: bounded_string(object.get("id"), "id")?,
            method: bounded_string(object.get("method"), "method")?,
            body: value,
        }),
        event_type => Ok(RpcRecord::Event {
            event_type: bounded(event_type, "type")?,
            body: value,
        }),
    }
}

fn bounded_string(value: Option<&Value>, field: &str) -> io::Result<String> {
    let value = value
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(format!("Pi RPC {field} must be a string")))?;
    bounded(value, field)
}

fn optional_bounded_string(value: Option<&Value>, field: &str) -> io::Result<Option<String>> {
    value
        .map(|value| bounded_string(Some(value), field))
        .transpose()
}

fn bounded(value: &str, field: &str) -> io::Result<String> {
    if value.is_empty() || value.len() > 4096 || value.chars().any(char::is_control) {
        return Err(invalid(format!("Pi RPC {field} is invalid")));
    }
    Ok(value.to_string())
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::Cursor,
        sync::mpsc,
        time::{Duration, Instant},
    };

    #[test]
    fn interleaved_response_ui_and_event_records_are_classified() {
        let input = concat!(
            "{\"type\":\"extension_ui_request\",\"id\":\"ui-1\",\"method\":\"setStatus\"}\n",
            "{\"id\":\"state\",\"type\":\"response\",\"command\":\"get_state\",\"success\":true,\"data\":{}}\n",
            "{\"type\":\"agent_settled\"}\n"
        );
        let mut reader = Cursor::new(input.as_bytes());
        assert!(
            matches!(read_record(&mut reader).unwrap(), Some(RpcRecord::ExtensionUi { method, .. }) if method == "setStatus")
        );
        assert!(
            matches!(read_record(&mut reader).unwrap(), Some(RpcRecord::Response { command, success: true, .. }) if command == "get_state")
        );
        assert!(
            matches!(read_record(&mut reader).unwrap(), Some(RpcRecord::Event { event_type, .. }) if event_type == "agent_settled")
        );
        assert_eq!(read_record(&mut reader).unwrap(), None);
    }

    #[test]
    fn unicode_separators_inside_json_do_not_split_records() {
        let mut reader =
            Cursor::new("{\"type\":\"message_update\",\"text\":\"a b c\"}\n".as_bytes());
        assert!(
            matches!(read_record(&mut reader).unwrap(), Some(RpcRecord::Event { event_type, .. }) if event_type == "message_update")
        );
    }

    #[test]
    fn malformed_incomplete_and_oversized_records_fail_closed() {
        for input in [b"[]\n".as_slice(), b"{bad}\n", b"{\"type\":\"agent_end\"}"] {
            assert_eq!(
                read_record(&mut Cursor::new(input)).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
        let mut oversized = vec![b'x'; MAX_RPC_RECORD_BYTES + 1];
        oversized.push(b'\n');
        assert_eq!(
            read_record(&mut Cursor::new(oversized)).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn process_serializes_commands_and_reaps_on_termination() {
        let plan = ConversationLaunchPlan {
            argv: vec!["/bin/cat".into()],
            capabilities: Default::default(),
        };
        let (sender, receiver) = mpsc::channel();
        let callback = Arc::new(move |record| {
            let _ = sender.send(record);
        });
        let mut process = ConversationProcess::spawn(&plan, Path::new("/"), callback).unwrap();
        process
            .send(&serde_json::json!({"type":"agent_settled"}))
            .unwrap();
        assert!(
            matches!(receiver.recv_timeout(Duration::from_secs(1)).unwrap(), RpcRecord::Event { event_type, .. } if event_type == "agent_settled")
        );
        process.terminate();
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(1)).unwrap(),
            RpcRecord::Exited
        ));
    }

    #[test]
    fn termination_reaps_descendants_without_waiting_for_inherited_pipes() {
        let plan = ConversationLaunchPlan {
            argv: vec!["/bin/sh".into(), "-c".into(), "sleep 20 & cat".into()],
            capabilities: Default::default(),
        };
        let mut process =
            ConversationProcess::spawn(&plan, Path::new("/"), Arc::new(|_| {})).unwrap();
        let start = Instant::now();
        process.terminate();
        assert!(start.elapsed() < Duration::from_secs(2));
    }
}
