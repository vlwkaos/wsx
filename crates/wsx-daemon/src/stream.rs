//! The frame and raw terminal streams of one controller.

use super::*;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum TerminalStreamKind {
    /// Rendered cell frames for the TUI.
    Frames,
    /// PTY bytes for a client that runs its own emulator (docs/terminal-raw-stream.md).
    Raw,
}

pub(crate) fn serve_terminal_stream(
    mut stream: UnixStream,
    daemon: Arc<Daemon>,
    kind: TerminalStreamKind,
    (pane_id, client_id, takeover): (PaneId, u64, bool),
    (rows, cols): (u16, u16),
) -> io::Result<()> {
    let (acquire_revision, lease_generation) =
        match acquire_terminal_lease(&daemon, pane_id, client_id, takeover) {
            Ok(acquired) => acquired,
            Err(error) => {
                write_response(&mut stream, Response::Error(error))?;
                return Ok(());
            }
        };
    if let Err(error) = touch_terminal_project(&daemon, pane_id) {
        release_stream_lease(&daemon, pane_id, client_id, lease_generation);
        write_response(&mut stream, Response::Error(error))?;
        return Ok(());
    }
    // A raw subscriber may keep the pane's size: it then scales its own view.
    let keep_size = kind == TerminalStreamKind::Raw && rows == 0 && cols == 0;
    let resized = if keep_size {
        Ok(())
    } else {
        resize_terminal(
            &daemon,
            pane_id,
            LeaseAccess::Stream {
                client_id,
                generation: lease_generation,
            },
            rows,
            cols,
        )
        .map(|_| ())
    };
    if let Err(error) = resized {
        release_stream_lease(&daemon, pane_id, client_id, lease_generation);
        write_response(&mut stream, Response::Error(error))?;
        return Ok(());
    }
    // Clipboard writes are effects for the client that was attached when they occurred.
    // Never replay writes captured before this stream became active.
    if let Err(error) =
        with_stream_runtime(&daemon, pane_id, client_id, lease_generation, |runtime| {
            let _ = runtime.take_clipboard_writes();
            Ok(())
        })
    {
        release_stream_lease(&daemon, pane_id, client_id, lease_generation);
        write_response(&mut stream, Response::Error(error))?;
        return Ok(());
    }
    write_response(
        &mut stream,
        Response::Ack {
            revision: acquire_revision,
        },
    )?;

    stream.set_read_timeout(None)?;
    let active = Arc::new(AtomicBool::new(true));
    let resync = Arc::new(AtomicBool::new(true));
    let input_error = Arc::new(Mutex::new(None::<ApiError>));
    let input_stream = stream.try_clone()?;
    let input_daemon = Arc::clone(&daemon);
    let input_active = Arc::clone(&active);
    let input_resync = Arc::clone(&resync);
    let input_error_slot = Arc::clone(&input_error);
    let input_thread = thread::Builder::new()
        .name("wsxd-terminal-input".into())
        .spawn(move || {
            let mut reader = BufReader::new(input_stream);
            loop {
                let message = match read_bounded_line(&mut reader) {
                    Ok(Some(line)) => match serde_json::from_str::<TerminalClientMessage>(&line) {
                        Ok(message) => message,
                        Err(error) => {
                            *lock(&input_error_slot) =
                                Some(ApiError::new("invalid_stream_message", error.to_string()));
                            break;
                        }
                    },
                    Ok(None) => break,
                    Err(error) => {
                        *lock(&input_error_slot) =
                            Some(ApiError::new("stream_read_failed", error.to_string()));
                        break;
                    }
                };
                match message {
                    TerminalClientMessage::Resync => {
                        input_resync.store(true, Ordering::Release);
                        input_daemon.changed.notify_all();
                        continue;
                    }
                    TerminalClientMessage::Detach => break,
                    message => {
                        if let Err(error) = handle_terminal_stream_input(
                            &input_daemon,
                            pane_id,
                            client_id,
                            lease_generation,
                            message,
                        ) {
                            *lock(&input_error_slot) = Some(error);
                            break;
                        }
                    }
                }
            }
            input_active.store(false, Ordering::Release);
            input_daemon.changed.notify_all();
        })?;

    let lease = StreamLease {
        pane_id,
        client_id,
        generation: lease_generation,
    };
    let result = match kind {
        TerminalStreamKind::Frames => {
            stream_terminal_updates(&mut stream, &daemon, lease, &active, &resync, &input_error)
        }
        TerminalStreamKind::Raw => {
            stream_raw_terminal_output(&mut stream, &daemon, lease, &active, &resync, &input_error)
        }
    };
    active.store(false, Ordering::Release);
    let _ = stream.shutdown(Shutdown::Both);
    let _ = input_thread.join();
    release_stream_lease(&daemon, pane_id, client_id, lease_generation);
    result
}

pub(crate) fn stream_terminal_updates(
    stream: &mut UnixStream,
    daemon: &Arc<Daemon>,
    lease: StreamLease,
    active: &AtomicBool,
    resync: &AtomicBool,
    input_error: &Mutex<Option<ApiError>>,
) -> io::Result<()> {
    let mut baseline = None;
    let mut last_frame_sent = Instant::now();
    while active.load(Ordering::Acquire) {
        // The handoff holds every pane operation lock, so tell the client before waiting on one.
        let handoff = terminal_stream_lease_error(&lock(&daemon.state), lease, Instant::now())
            .filter(|error| error.code == "daemon_handoff");
        if let Some(error) = handoff {
            write_stream_message(stream, &TerminalServerMessage::Error(error))?;
            break;
        }
        // ^ docs/terminal-raw-stream.md: messages are sampled under the pane operation
        // lock and written after it, so a slow reader cannot delay input, resize or a takeover.
        let mut messages = Vec::new();
        let mut ended = false;
        let operation_lock = terminal_operation_lock(daemon, lease.pane_id);
        let operation = lock(&operation_lock);
        let runtime = match leased_stream_runtime(&lock(&daemon.state), lease) {
            Ok(runtime) => runtime,
            Err(error) => {
                drop(operation);
                write_stream_message(stream, &TerminalServerMessage::Error(error))?;
                break;
            }
        };
        let exited = runtime.exited();
        if resync.swap(false, Ordering::AcqRel) {
            baseline = None;
        }
        let frame_due = baseline.is_none() || last_frame_sent.elapsed() >= PRESENTATION_CADENCE;
        let sample = runtime.presentation_sample(baseline, frame_due);
        let sample_revision = sample.revision;
        let synchronized_output = sample.synchronized_output;
        messages.extend(
            sample
                .clipboard_writes
                .into_iter()
                .map(TerminalServerMessage::ClipboardWrite),
        );
        let mut frame_sent = false;
        if let Some(error) = lock(input_error).take() {
            messages.push(TerminalServerMessage::Error(error));
            ended = true;
        } else if exited {
            messages.push(TerminalServerMessage::Exited);
            ended = true;
        } else {
            match sample.update {
                Ok(Some(update)) => {
                    baseline = Some(update.revision());
                    messages.push(TerminalServerMessage::Update(update));
                    frame_sent = true;
                }
                Ok(None) => {}
                Err(error) => {
                    messages.push(TerminalServerMessage::Error(ApiError::new(
                        "frame_failed",
                        error.to_string(),
                    )));
                    ended = true;
                }
            }
        }
        drop(operation);
        for message in &messages {
            write_stream_message(stream, message)?;
        }
        if frame_sent {
            last_frame_sent = Instant::now();
        }
        if ended {
            break;
        }
        let state = lock(&daemon.state);
        // ^ [[wsx Architecture]] Recheck every wake predicate while holding the
        // notifier mutex. Frames wait only for the bounded presentation cadence;
        // effects bypass it in the atomically sampled FIFO above.
        if !active.load(Ordering::Acquire) || resync.load(Ordering::Acquire) {
            continue;
        }
        let current_revision = runtime.revision();
        if synchronized_output && sample_revision != current_revision {
            continue;
        }
        let wait = if !synchronized_output && baseline != Some(current_revision) {
            PRESENTATION_CADENCE.saturating_sub(last_frame_sent.elapsed())
        } else {
            Duration::from_millis(250)
        };
        if wait.is_zero() {
            continue;
        }
        let _ = daemon
            .changed
            .wait_timeout(state, wait)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
    }
    Ok(())
}

// ^ docs/terminal-raw-stream.md: a replay, then the PTY bytes after it. A subscriber
// that falls behind the tap gets a new replay instead of a gap. Messages are sampled
// under the pane operation lock and written after it, so a slow reader cannot delay
// a takeover of the pane.
pub(crate) fn stream_raw_terminal_output(
    stream: &mut UnixStream,
    daemon: &Arc<Daemon>,
    lease: StreamLease,
    active: &AtomicBool,
    resync: &AtomicBool,
    input_error: &Mutex<Option<ApiError>>,
) -> io::Result<()> {
    let mut position = None::<u64>;
    let mut size = (0, 0);
    while active.load(Ordering::Acquire) {
        // The handoff holds every pane operation lock, so tell the client before waiting on one.
        let handoff = terminal_stream_lease_error(&lock(&daemon.state), lease, Instant::now())
            .filter(|error| error.code == "daemon_handoff");
        if let Some(error) = handoff {
            write_stream_message(stream, &TerminalRawServerMessage::Error(error))?;
            break;
        }
        let mut messages = Vec::new();
        let mut ended = false;
        let operation_lock = terminal_operation_lock(daemon, lease.pane_id);
        let operation = lock(&operation_lock);
        let runtime = match leased_stream_runtime(&lock(&daemon.state), lease) {
            Ok(runtime) => runtime,
            Err(error) => {
                drop(operation);
                write_stream_message(stream, &TerminalRawServerMessage::Error(error))?;
                break;
            }
        };
        // Read the exit flag and the revision first: bytes ingested before the exit
        // are still sent, and a later byte or resize changes the revision.
        let exited = runtime.exited();
        let revision = runtime.revision();
        if resync.swap(false, Ordering::AcqRel) {
            position = None;
        }
        let mut drained = false;
        // ^ Bytes of an open synchronized update wait for its end, so a client emulator without
        // mode 2026 (xterm.js 5.5) never draws half an update (docs/terminal-synchronized-output.md).
        let mut holding = false;
        let sampled = match position {
            None => runtime.raw_replay().map(|replay| {
                position = Some(replay.sequence);
                size = (replay.rows, replay.cols);
                messages.push(TerminalRawServerMessage::Replay(TerminalRawReplay {
                    ansi: replay.ansi,
                    rows: replay.rows,
                    cols: replay.cols,
                    truncated: replay.truncated,
                }));
            }),
            Some(after) => runtime.raw_sample(after).map(|sample| {
                messages.extend(
                    sample
                        .clipboard_writes
                        .into_iter()
                        .map(TerminalRawServerMessage::ClipboardWrite),
                );
                if (sample.rows, sample.cols) != size {
                    // Two emulators reflow a resized screen differently, so the
                    // client restarts from this one's screen: a replay follows.
                    size = (sample.rows, sample.cols);
                    messages.push(TerminalRawServerMessage::Resized {
                        rows: sample.rows,
                        cols: sample.cols,
                    });
                    position = None;
                    return;
                }
                match sample.output {
                    None => position = None,
                    Some(mut bytes) => {
                        if sample.synchronized_output {
                            let open = synchronized_update_start(&bytes).unwrap_or(0);
                            holding = open < bytes.len();
                            bytes.truncate(open);
                        }
                        drained = bytes.is_empty() && !holding;
                        position = Some(after + bytes.len() as u64);
                        if !bytes.is_empty() {
                            messages.push(TerminalRawServerMessage::Output(bytes));
                        }
                    }
                }
            }),
        };
        if let Err(error) = sampled {
            messages.push(TerminalRawServerMessage::Error(ApiError::new(
                "replay_failed",
                error.to_string(),
            )));
            ended = true;
        } else if let Some(error) = lock(input_error).take() {
            messages.push(TerminalRawServerMessage::Error(error));
            ended = true;
        } else if exited && drained {
            messages.push(TerminalRawServerMessage::Exited);
            ended = true;
        }
        drop(operation);
        for message in &messages {
            write_stream_message(stream, message)?;
        }
        if ended {
            break;
        }
        let state = lock(&daemon.state);
        // Recheck every wake predicate while holding the notifier mutex.
        if !active.load(Ordering::Acquire)
            || resync.load(Ordering::Acquire)
            || (!drained && !holding)
            || runtime.exited() != exited
            || runtime.revision() != revision
        {
            continue;
        }
        // ^ A held update is sent when it ends, which new output wakes, or when its bound
        // expires, which nothing wakes: look again soon.
        let wait = if holding {
            SYNCHRONIZED_HOLD_POLL
        } else {
            Duration::from_millis(250)
        };
        let _ = daemon
            .changed
            .wait_timeout(state, wait)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
    }
    Ok(())
}

pub(crate) const SYNCHRONIZED_HOLD_POLL: Duration = Duration::from_millis(50);

/// Where the open synchronized update (`CSI ? 2026 h`) starts in these bytes, if it does:
/// the bytes before it are complete and may go now.
pub(crate) fn synchronized_update_start(bytes: &[u8]) -> Option<usize> {
    const START: &[u8] = b"\x1b[?2026h";
    bytes
        .windows(START.len())
        .rposition(|window| window == START)
}

/// The runtime of a stream that still holds its pane lease.
pub(crate) fn leased_stream_runtime(
    state: &State,
    lease: StreamLease,
) -> Result<Arc<TerminalRuntime>, ApiError> {
    if let Some(error) = terminal_stream_lease_error(state, lease, Instant::now()) {
        return Err(error);
    }
    state.runtimes.get(&lease.pane_id).cloned().ok_or_else(|| {
        ApiError::new(
            "lease_lost",
            "terminal stream no longer owns the pane lease",
        )
    })
}

pub(crate) fn write_stream_message(
    stream: &mut UnixStream,
    message: &impl Serialize,
) -> io::Result<()> {
    let bytes = encode_line(message).map_err(io::Error::other)?;
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "terminal update exceeds limit",
        ));
    }
    stream.write_all(&bytes)
}

pub(crate) fn with_stream_runtime<T>(
    daemon: &Daemon,
    pane_id: PaneId,
    client_id: u64,
    lease_generation: u64,
    operation: impl FnOnce(&TerminalRuntime) -> Result<T, ApiError>,
) -> Result<T, ApiError> {
    let operation_lock = terminal_operation_lock(daemon, pane_id);
    let _operation = lock(&operation_lock);
    let (runtime, _) = leased_runtime(
        daemon,
        pane_id,
        LeaseAccess::Stream {
            client_id,
            generation: lease_generation,
        },
    )?;
    operation(&runtime)
}

pub(crate) fn handle_terminal_stream_input(
    daemon: &Daemon,
    pane_id: PaneId,
    client_id: u64,
    lease_generation: u64,
    message: TerminalClientMessage,
) -> Result<(), ApiError> {
    match message {
        TerminalClientMessage::Input(bytes) => {
            if bytes.len() > MAX_INPUT_BYTES {
                return Err(api("invalid_input", "input exceeds limit"));
            }
            with_stream_runtime(daemon, pane_id, client_id, lease_generation, |runtime| {
                runtime.write(&bytes).map_err(terminal_api)
            })?;
            request_claude_reconcile(daemon, pane_id, bytes == [3]);
            Ok(())
        }
        TerminalClientMessage::Key(key) => {
            with_stream_runtime(daemon, pane_id, client_id, lease_generation, |runtime| {
                runtime.key(&key).map_err(terminal_api)
            })?;
            // Claude has no interrupt hook. Input only requests a fresh terminal
            // observation; current provider UI evidence decides the resulting state.
            // ^ https://code.claude.com/docs/en/hooks#stop
            request_claude_reconcile(daemon, pane_id, key_interrupts_agent(&key));
            Ok(())
        }
        TerminalClientMessage::Paste(text) => {
            if text.len() > MAX_INPUT_BYTES {
                return Err(api("invalid_input", "paste exceeds limit"));
            }
            with_stream_runtime(daemon, pane_id, client_id, lease_generation, |runtime| {
                runtime.paste(&text).map_err(terminal_api)
            })
        }
        TerminalClientMessage::Mouse(mouse) => {
            with_stream_runtime(daemon, pane_id, client_id, lease_generation, |runtime| {
                runtime.mouse(&mouse).map_err(terminal_api)
            })
        }
        TerminalClientMessage::Resize { rows, cols } => {
            resize_terminal(
                daemon,
                pane_id,
                LeaseAccess::Stream {
                    client_id,
                    generation: lease_generation,
                },
                rows,
                cols,
            )?;
            Ok(())
        }
        TerminalClientMessage::Heartbeat => {
            let mut state = lock(&daemon.state);
            refresh_lease(
                &mut state,
                pane_id,
                LeaseAccess::Stream {
                    client_id,
                    generation: lease_generation,
                },
            )
        }
        TerminalClientMessage::Resync | TerminalClientMessage::Detach => Err(api(
            "invalid_stream_message",
            "stream control message reached the input handler",
        )),
    }
}
