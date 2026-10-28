//! Persistent PTY runtime backed by pinned libghostty-vt.
// ^ [[wsx Architecture]] The daemon owns this type; clients only receive frames.

mod ghostty;
mod handoff;
mod mouse;
mod presentation;

use handoff::bounded_handoff_ansi;
pub use handoff::{TerminalHandoffState, MAX_HANDOFF_ANSI_BYTES};
use mouse::{clear_local_selection, ChildMouseGesture, SelectionAutoscrollState};
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
pub use presentation::{AgentTerminalObservation, PresentationSample, RawReplay, RawSample};
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    error::Error,
    fmt,
    io::{self, Write},
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc, Condvar, Mutex, MutexGuard,
    },
    thread,
    time::{Duration, Instant},
};
use std::{
    os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd},
    os::unix::net::UnixStream,
};
use wsx_core::runtime::{
    Cell, CellModifiers, Cursor, KeyCode, KeyEvent, MouseAction, MouseButton, MouseEvent, PaneId,
    TerminalFrame, TerminalId, TerminalRowPatch, TerminalSelectionRange, TerminalUpdate,
};

const MAX_SCROLLBACK_ROWS: usize = 10_000;
const MAX_ROWS: u16 = 1_000;
const MAX_COLS: u16 = 1_000;
const MAX_CELLS: usize = 100_000;
const MAX_ARGS: usize = 256;
const MAX_ARG_BYTES: usize = 64 * 1024;
const MAX_STARTUP_INPUT_BYTES: usize = 64 * 1024;
const STARTUP_WRITE_TIMEOUT: Duration = Duration::from_secs(2);
const QUERY_REPLY_QUEUE: usize = 256;
/// docs/terminal-raw-stream.md "Input": the longest a write waits for a program that does
/// not read its input. Stream heartbeats share the input thread and expire after 3 s.
const INPUT_WRITE_LIMIT: Duration = Duration::from_secs(1);
const BRACKETED_PASTE_MODE: u16 = 2004;
const SYNCHRONIZED_OUTPUT_TIMEOUT: Duration = Duration::from_secs(1);
// ^ docs/terminal-raw-stream.md: bounds of the PTY byte tap and of one stream message.
const RAW_TAP_BYTES: usize = 1024 * 1024;

#[derive(Debug)]
pub enum TerminalError {
    InvalidDimensions { rows: u16, cols: u16 },
    InvalidCommand,
    Pty(String),
    Io(io::Error),
    Ghostty(ghostty::Error),
    Runtime(String),
}

impl fmt::Display for TerminalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDimensions { rows, cols } => {
                write!(f, "unsupported terminal dimensions {cols}x{rows}")
            }
            Self::InvalidCommand => f.write_str("invalid terminal command argv"),
            Self::Pty(message) => write!(f, "PTY error: {message}"),
            Self::Io(error) => write!(f, "terminal I/O error: {error}"),
            Self::Ghostty(error) => write!(f, "terminal emulation error: {error}"),
            Self::Runtime(message) => f.write_str(message),
        }
    }
}
impl Error for TerminalError {}
impl From<io::Error> for TerminalError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}
impl From<ghostty::Error> for TerminalError {
    fn from(value: ghostty::Error) -> Self {
        Self::Ghostty(value)
    }
}

struct Emulator {
    terminal: ghostty::Terminal,
    render: ghostty::RenderState,
    keys: ghostty::KeyEncoder,
    mouse: ghostty::MouseEncoder,
    local_selection_active: bool,
    selection_autoscroll: Option<SelectionAutoscrollState>,
    selection_autoscroll_worker_running: bool,
    child_mouse_gesture: Option<ChildMouseGesture>,
    synchronized_since: Option<Instant>,
    raw: RawTap,
}

/// PTY output as the child wrote it, for clients that run their own emulator.
/// `end` counts every ingested byte. The ring exists from the first replay on.
#[derive(Default)]
struct RawTap {
    end: u64,
    ring: Option<VecDeque<u8>>,
}

impl RawTap {
    fn push(&mut self, bytes: &[u8]) {
        self.end += bytes.len() as u64;
        let Some(ring) = &mut self.ring else {
            return;
        };
        let bytes = &bytes[bytes.len().saturating_sub(RAW_TAP_BYTES)..];
        let overflow = (ring.len() + bytes.len()).saturating_sub(RAW_TAP_BYTES);
        ring.drain(..overflow);
        ring.extend(bytes);
    }

    /// Bytes after `after`, or `None` when the ring no longer holds them.
    fn read(&self, after: u64, max_bytes: usize) -> Option<Vec<u8>> {
        let ring = self.ring.as_ref()?;
        let start = self.end - ring.len() as u64;
        if after < start || after > self.end {
            return None;
        }
        let skip = (after - start) as usize;
        let (head, tail) = ring.as_slices();
        let mut bytes = Vec::with_capacity((ring.len() - skip).min(max_bytes));
        if skip < head.len() {
            let head = &head[skip..];
            bytes.extend_from_slice(&head[..head.len().min(max_bytes)]);
        }
        let taken = bytes.len();
        if taken < max_bytes {
            let tail = &tail[skip.saturating_sub(head.len())..];
            bytes.extend_from_slice(&tail[..tail.len().min(max_bytes - taken)]);
        }
        Some(bytes)
    }
}

impl Emulator {
    fn sync_input(&mut self) {
        self.keys.set_from_terminal(&self.terminal);
        self.mouse.set_from_terminal(&self.terminal);
    }

    fn ingest(&mut self, bytes: &[u8]) {
        self.raw.push(bytes);
        self.terminal.write(bytes);
        self.sync_input();
        self.synchronized_output(Instant::now());
    }

    // ^ docs/terminal-synchronized-output.md: a program that dies mid-frame never
    // resets mode 2026, so the mode expires instead of hiding all later output.
    fn synchronized_output(&mut self, now: Instant) -> bool {
        if !self
            .terminal
            .mode_get(ghostty::MODE_SYNCHRONIZED_OUTPUT)
            .unwrap_or(false)
        {
            self.synchronized_since = None;
            return false;
        }
        let since = *self.synchronized_since.get_or_insert(now);
        if now.saturating_duration_since(since) < SYNCHRONIZED_OUTPUT_TIMEOUT {
            return true;
        }
        self.synchronized_since = None;
        let _ = self
            .terminal
            .mode_set(ghostty::MODE_SYNCHRONIZED_OUTPUT, false);
        false
    }
}

struct Process {
    master: Mutex<Box<dyn MasterPty + Send>>,
    killer: Mutex<Option<Box<dyn ChildKiller + Send + Sync>>>,
    process_group: Arc<std::sync::atomic::AtomicI32>,
}

#[derive(Default)]
struct ReaderState {
    pause_requested: bool,
    paused: bool,
}

#[derive(Default)]
struct ReaderControl {
    state: Mutex<ReaderState>,
    changed: Condvar,
    wake: Mutex<Option<UnixStream>>,
}

struct Shared {
    emulator: Mutex<Emulator>,
    writer: Arc<Mutex<Option<Box<dyn Write + Send>>>>,
    error: Arc<Mutex<Option<String>>>,
    revision: AtomicU64,
    exited: AtomicBool,
    terminating: AtomicBool,
    notify: Arc<dyn Fn() + Send + Sync>,
    reader: Arc<ReaderControl>,
}

pub struct TerminalRuntime {
    pane_id: PaneId,
    terminal_id: TerminalId,
    shared: Arc<Shared>,
    process: Option<Process>,
    selection_clock: Instant,
    preserve_process_on_drop: AtomicBool,
}

impl TerminalRuntime {
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        pane_id: PaneId,
        terminal_id: TerminalId,
        cwd: &Path,
        command: &[String],
        environment: &[(String, String)],
        initial_input: Option<&[u8]>,
        rows: u16,
        cols: u16,
        notify: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Self, TerminalError> {
        validate_launch(rows, cols, command, initial_input)?;
        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|error| TerminalError::Pty(error.to_string()))?;
        let reader_fd = duplicate_fd(
            pair.master
                .as_raw_fd()
                .ok_or_else(|| TerminalError::Runtime("PTY has no file descriptor".into()))?,
        )?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| TerminalError::Pty(e.to_string()))?;
        let writer = Arc::new(Mutex::new(Some(writer)));
        let error = Arc::new(Mutex::new(None));
        let emulator = make_emulator(cols, rows, Arc::clone(&writer), Arc::clone(&error))?;

        let mut builder = CommandBuilder::new(&command[0]);
        builder.args(&command[1..]);
        builder.cwd(cwd);
        builder.env("TERM", "xterm-ghostty");
        // ^ A fresh wsxd-owned PTY is not a Claude child session, even when
        // wsxd inherited this marker at bootstrap. An explicit pane override
        // can still pass it for a deliberately nested agent launch.
        builder.env_remove("CLAUDE_CODE_CHILD_SESSION");
        for (name, value) in environment {
            builder.env(name, value);
        }
        let child = pair
            .slave
            .spawn_command(builder)
            .map_err(|e| TerminalError::Pty(e.to_string()))?;
        let mut killer = child.clone_killer();
        let process_group = pair.master.process_group_leader();
        let process_group_owner = Arc::new(std::sync::atomic::AtomicI32::new(
            process_group.unwrap_or_default(),
        ));
        if let Some(bytes) = initial_input.filter(|bytes| !bytes.is_empty()) {
            if let Err(error) = write_startup_input(pair.master.as_ref(), bytes) {
                abort_spawn(&mut killer, process_group);
                return Err(TerminalError::Io(error));
            }
        }
        let (reader, reader_wake) = reader_control()?;
        let shared = Arc::new(Shared {
            emulator: Mutex::new(emulator),
            writer,
            error,
            revision: AtomicU64::new(1),
            exited: AtomicBool::new(false),
            terminating: AtomicBool::new(false),
            notify,
            reader,
        });
        let reader_result = spawn_reader_fd(reader_fd, reader_wake, Arc::clone(&shared));
        if let Err(error) = reader_result {
            abort_spawn(&mut killer, process_group);
            return Err(error);
        }
        if let Err(error) =
            spawn_waiter(child, Arc::clone(&shared), Arc::clone(&process_group_owner))
        {
            abort_spawn(&mut killer, process_group);
            return Err(error);
        }
        Ok(Self {
            pane_id,
            terminal_id,
            shared,
            process: Some(Process {
                master: Mutex::new(pair.master),
                killer: Mutex::new(Some(killer)),
                process_group: process_group_owner,
            }),
            selection_clock: Instant::now(),
            preserve_process_on_drop: AtomicBool::new(false),
        })
    }

    pub fn process_group_id(&self) -> Option<libc::pid_t> {
        self.process.as_ref().and_then(|process| {
            let group = process.process_group.load(Ordering::Acquire);
            (group > 1).then_some(group)
        })
    }

    pub fn has_foreground_job(&self) -> bool {
        let Some(process) = self.process.as_ref() else {
            return false;
        };
        let root_group = process.process_group.load(Ordering::Acquire);
        if root_group <= 1 {
            return false;
        }
        let foreground_group = lock(&process.master).process_group_leader();
        foreground_group.is_some_and(|group| group > 1 && group != root_group)
    }

    /// `write_within` the `INPUT_WRITE_LIMIT`: a program that stops reading its input
    /// fails the write with `TimedOut` instead of holding the caller and its locks.
    pub fn write(&self, bytes: &[u8]) -> Result<(), TerminalError> {
        self.write_within(bytes, Instant::now() + INPUT_WRITE_LIMIT)
    }

    pub fn key(&self, key: &KeyEvent) -> Result<(), TerminalError> {
        if key.text.len() > 16 {
            return Err(TerminalError::Runtime("key text exceeds limit".into()));
        }
        let bytes = {
            let mut emulator = lock(&self.shared.emulator);
            encode_key(&mut emulator, key)?
        };
        self.write(&bytes)
    }

    /// `write` that gives up at `deadline` when the program stops reading its input.
    /// Bytes written before the deadline stay in the program's input.
    pub fn write_within(&self, bytes: &[u8], deadline: Instant) -> Result<(), TerminalError> {
        if let Some(process) = self.process.as_ref().filter(|_| !bytes.is_empty()) {
            if self.exited() {
                return Err(TerminalError::Runtime("terminal process exited".into()));
            }
            // ^ Hold the writer: no other write may see the master in non-blocking mode.
            let writer = lock(&self.shared.writer);
            if writer.is_none() {
                return Err(TerminalError::Io(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "PTY closed",
                )));
            }
            let master = lock(&process.master);
            return write_master_until(master.as_ref(), bytes, deadline).map_err(TerminalError::Io);
        }
        let _ = deadline;
        if bytes.is_empty() {
            return Ok(());
        }
        if self.exited() {
            return Err(TerminalError::Runtime("terminal process exited".into()));
        }
        write_pty(&self.shared.writer, bytes).map_err(TerminalError::Io)
    }

    pub fn paste(&self, text: &str) -> Result<(), TerminalError> {
        self.write(&self.paste_bytes(text)?)
    }

    /// `paste` that gives up at `deadline`, as `write_within` does.
    pub fn paste_within(&self, text: &str, deadline: Instant) -> Result<(), TerminalError> {
        self.write_within(&self.paste_bytes(text)?, deadline)
    }

    fn paste_bytes(&self, text: &str) -> Result<Vec<u8>, TerminalError> {
        let emulator = lock(&self.shared.emulator);
        Ok(ghostty::encode_paste(
            text.as_bytes(),
            emulator.terminal.mode_get(BRACKETED_PASTE_MODE)?,
        )?)
    }

    pub fn resize(&self, rows: u16, cols: u16) -> Result<(), TerminalError> {
        validate_dimensions(rows, cols)?;
        let process = self
            .process
            .as_ref()
            .ok_or_else(|| TerminalError::Runtime("terminal has no PTY".into()))?;
        lock(&process.master)
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| TerminalError::Pty(e.to_string()))?;
        let mut emulator = lock(&self.shared.emulator);
        emulator.terminal.resize(cols, rows, 1, 1)?;
        clear_local_selection(&mut emulator)?;
        emulator
            .mouse
            .set_size(u32::from(cols), u32::from(rows), 1, 1);
        emulator.sync_input();
        self.shared.revision.fetch_add(1, Ordering::AcqRel);
        drop(emulator);
        (self.shared.notify)();
        Ok(())
    }

    pub fn take_clipboard_writes(&self) -> Vec<Vec<u8>> {
        lock(&self.shared.emulator).terminal.take_clipboard_writes()
    }

    pub fn revision(&self) -> u64 {
        self.shared.revision.load(Ordering::Acquire)
    }

    #[cfg(test)]
    fn synchronized_output_active(&self) -> bool {
        let emulator = lock(&self.shared.emulator);
        emulator
            .terminal
            .mode_get(ghostty::MODE_SYNCHRONIZED_OUTPUT)
            .unwrap_or(false)
    }

    pub fn exited(&self) -> bool {
        self.shared.exited.load(Ordering::Acquire)
    }

    pub fn terminate(&self) {
        if self.shared.terminating.swap(true, Ordering::AcqRel) {
            return;
        }
        {
            let mut writer = lock(&self.shared.writer);
            // ^ portable-pty's writer writes an end-of-file byte when it is dropped. A program
            // that stopped reading its input would hold that write, and wsxd with it, for ever.
            if let Some(fd) = self
                .process
                .as_ref()
                .and_then(|process| lock(&process.master).as_raw_fd())
            {
                unsafe {
                    let flags = libc::fcntl(fd, libc::F_GETFL);
                    if flags >= 0 {
                        libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
                    }
                }
            }
            writer.take();
        }
        if let Some(process) = &self.process {
            terminate_group(take_process_group(&process.process_group));
            if let Some(killer) = lock(&process.killer).as_mut() {
                let _ = killer.kill();
            }
        }
        mark_exited(&self.shared);
    }

    #[cfg(test)]
    fn new_for_test(rows: u16, cols: u16) -> Result<Self, TerminalError> {
        let writer: Arc<Mutex<Option<Box<dyn Write + Send>>>> =
            Arc::new(Mutex::new(Some(Box::new(io::sink()))));
        let error = Arc::new(Mutex::new(None));
        let emulator = make_emulator(cols, rows, Arc::clone(&writer), Arc::clone(&error))?;
        Ok(Self {
            pane_id: PaneId(1),
            terminal_id: TerminalId(2),
            selection_clock: Instant::now(),
            shared: Arc::new(Shared {
                emulator: Mutex::new(emulator),
                writer,
                error,
                revision: AtomicU64::new(1),
                exited: AtomicBool::new(false),
                terminating: AtomicBool::new(false),
                notify: Arc::new(|| {}),
                reader: Arc::new(ReaderControl::default()),
            }),
            process: None,
            preserve_process_on_drop: AtomicBool::new(false),
        })
    }
}
impl Drop for TerminalRuntime {
    fn drop(&mut self) {
        if self.preserve_process_on_drop.load(Ordering::Acquire) {
            if let Some(writer) = lock(&self.shared.writer).take() {
                std::mem::forget(writer);
            }
            if let Some(process) = self.process.take() {
                std::mem::forget(process);
            }
            return;
        }
        self.terminate();
    }
}

fn make_emulator(
    cols: u16,
    rows: u16,
    writer: Arc<Mutex<Option<Box<dyn Write + Send>>>>,
    error: Arc<Mutex<Option<String>>>,
) -> Result<Emulator, TerminalError> {
    let mut terminal = ghostty::Terminal::new(cols, rows, MAX_SCROLLBACK_ROWS)?;
    // ^ The callback runs inside `terminal.write` with the emulator lock held, on the reader
    // thread. A blocking PTY write there deadlocks with a child that stopped reading its input
    // and blocks on its output: the reader cannot drain, the child cannot read. One thread
    // writes the replies in order instead; the callback only queues them.
    let (replies, queued) = mpsc::sync_channel::<Vec<u8>>(QUERY_REPLY_QUEUE);
    thread::Builder::new()
        .name("wsx-terminal-reply".into())
        .spawn(move || {
            for reply in queued {
                if let Err(write_error) = write_pty(&writer, &reply) {
                    set_error(
                        &error,
                        format!("terminal query response failed: {write_error}"),
                    );
                }
            }
        })?;
    terminal.set_write_pty_callback(move |bytes| {
        // A full queue means the child reads no input and cannot use a reply: drop it
        // rather than block the reader.
        let _ = replies.try_send(bytes.to_vec());
    })?;
    let render = ghostty::RenderState::new()?;
    let mut keys = ghostty::KeyEncoder::new()?;
    keys.set_from_terminal(&terminal);
    let mut mouse = ghostty::MouseEncoder::new()?;
    mouse.set_from_terminal(&terminal);
    mouse.set_size(u32::from(cols), u32::from(rows), 1, 1);
    Ok(Emulator {
        terminal,
        render,
        keys,
        mouse,
        local_selection_active: false,
        selection_autoscroll: None,
        selection_autoscroll_worker_running: false,
        child_mouse_gesture: None,
        synchronized_since: None,
        raw: RawTap::default(),
    })
}

fn duplicate_fd(fd: RawFd) -> io::Result<OwnedFd> {
    let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
    if duplicate < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(duplicate) })
    }
}

fn reader_control() -> io::Result<(Arc<ReaderControl>, UnixStream)> {
    let (wake_reader, wake_writer) = UnixStream::pair()?;
    let control = Arc::new(ReaderControl {
        state: Mutex::new(ReaderState::default()),
        changed: Condvar::new(),
        wake: Mutex::new(Some(wake_writer)),
    });
    Ok((control, wake_reader))
}

fn spawn_reader_fd(
    reader: OwnedFd,
    wake: UnixStream,
    shared: Arc<Shared>,
) -> Result<(), TerminalError> {
    thread::Builder::new()
        .name("wsx-pty-reader".into())
        .spawn(move || {
            let fd = reader.as_raw_fd();
            let wake_fd = wake.as_raw_fd();
            let mut buffer = [0_u8; 16 * 1024];
            loop {
                {
                    let mut state = lock(&shared.reader.state);
                    if state.pause_requested {
                        state.paused = true;
                        shared.reader.changed.notify_all();
                        while state.pause_requested {
                            state = shared
                                .reader
                                .changed
                                .wait(state)
                                .unwrap_or_else(|poisoned| poisoned.into_inner());
                        }
                        state.paused = false;
                        shared.reader.changed.notify_all();
                    }
                }

                let mut pollfds = [
                    libc::pollfd {
                        fd,
                        events: libc::POLLIN | libc::POLLHUP,
                        revents: 0,
                    },
                    libc::pollfd {
                        fd: wake_fd,
                        events: libc::POLLIN | libc::POLLHUP,
                        revents: 0,
                    },
                ];
                let ready = unsafe { libc::poll(pollfds.as_mut_ptr(), pollfds.len() as _, -1) };
                if ready < 0 {
                    let error = io::Error::last_os_error();
                    if error.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    set_error(&shared.error, format!("PTY poll failed: {error}"));
                    break;
                }
                if pollfds[1].revents != 0 {
                    let mut byte = 0_u8;
                    let _ = unsafe { libc::read(wake_fd, (&mut byte as *mut u8).cast(), 1) };
                    continue;
                }
                let count = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), buffer.len()) };
                if count > 0 {
                    let mut emulator = lock(&shared.emulator);
                    emulator.ingest(&buffer[..count as usize]);
                    shared.revision.fetch_add(1, Ordering::AcqRel);
                    drop(emulator);
                    (shared.notify)();
                    continue;
                }
                if count == 0 {
                    break;
                }
                let error = io::Error::last_os_error();
                // ^ WouldBlock: a bounded write holds the shared master in non-blocking mode.
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                ) {
                    continue;
                }
                if error.raw_os_error() == Some(libc::EIO) {
                    break;
                }
                set_error(&shared.error, format!("PTY reader failed: {error}"));
                break;
            }
            mark_exited(&shared);
        })
        .map(|_| ())
        .map_err(TerminalError::Io)
}

fn spawn_waiter(
    mut child: Box<dyn portable_pty::Child + Send + Sync>,
    shared: Arc<Shared>,
    process_group: Arc<std::sync::atomic::AtomicI32>,
) -> Result<thread::JoinHandle<()>, TerminalError> {
    thread::Builder::new()
        .name("wsx-child-waiter".into())
        .spawn(move || {
            if let Err(error) = child.wait() {
                set_error(&shared.error, format!("child wait failed: {error}"));
                (shared.notify)();
            }
            terminate_group(take_process_group(&process_group));
            // ^ docs/terminal-exit-ordering.md: reader EOF owns natural exit so final PTY
            // output and clipboard effects precede stream Exited.
        })
        .map_err(TerminalError::Io)
}

fn abort_spawn(
    killer: &mut Box<dyn ChildKiller + Send + Sync>,
    process_group: Option<libc::pid_t>,
) {
    terminate_group(process_group);
    let _ = killer.kill();
}

fn take_process_group(group: &std::sync::atomic::AtomicI32) -> Option<libc::pid_t> {
    let group = group.swap(0, Ordering::AcqRel);
    (group > 1).then_some(group)
}

fn terminate_group(group: Option<libc::pid_t>) {
    if let Some(group) = group.filter(|group| *group > 1) {
        if group != unsafe { libc::getpgrp() } {
            unsafe {
                libc::kill(-group, libc::SIGTERM);
                libc::kill(-group, libc::SIGKILL);
            }
        }
    }
}

fn write_startup_input(master: &dyn MasterPty, bytes: &[u8]) -> io::Result<()> {
    write_master_until(master, bytes, Instant::now() + STARTUP_WRITE_TIMEOUT)
}

/// Write to the PTY master and give up at `deadline` when the program stops reading.
/// ^ O_NONBLOCK belongs to the open file, which the reader's duplicate shares: callers
/// hold the writer lock, and the reader takes EAGAIN as "poll again".
fn write_master_until(master: &dyn MasterPty, bytes: &[u8], deadline: Instant) -> io::Result<()> {
    let fd = master
        .as_raw_fd()
        .ok_or_else(|| io::Error::new(io::ErrorKind::Unsupported, "PTY has no file descriptor"))?;
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    struct RestoreFlags {
        fd: libc::c_int,
        flags: libc::c_int,
    }
    impl Drop for RestoreFlags {
        fn drop(&mut self) {
            unsafe {
                libc::fcntl(self.fd, libc::F_SETFL, self.flags);
            }
        }
    }
    let _restore = RestoreFlags { fd, flags };
    let mut written = 0;
    while written < bytes.len() {
        let count =
            unsafe { libc::write(fd, bytes[written..].as_ptr().cast(), bytes.len() - written) };
        if count > 0 {
            written += count as usize;
            continue;
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        if error.kind() != io::ErrorKind::WouldBlock {
            return Err(error);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "timed out writing terminal input",
            ));
        }
        let mut pollfd = libc::pollfd {
            fd,
            events: libc::POLLOUT,
            revents: 0,
        };
        let timeout = remaining.as_millis().min(100) as libc::c_int;
        let ready = unsafe { libc::poll(&mut pollfd, 1, timeout.max(1)) };
        if ready < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn write_pty(writer: &Mutex<Option<Box<dyn Write + Send>>>, bytes: &[u8]) -> io::Result<()> {
    let mut writer = lock(writer);
    let writer = writer
        .as_mut()
        .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "PTY closed"))?;
    writer.write_all(bytes)?;
    writer.flush()
}
fn set_error(slot: &Mutex<Option<String>>, message: String) {
    let mut slot = lock(slot);
    if slot.is_none() {
        *slot = Some(message);
    }
}
fn mark_exited(shared: &Shared) {
    let selection_changed = {
        let mut emulator = lock(&shared.emulator);
        clear_local_selection(&mut emulator).unwrap_or(false)
    };
    if !shared.exited.swap(true, Ordering::AcqRel) || selection_changed {
        shared.revision.fetch_add(1, Ordering::AcqRel);
        (shared.notify)();
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub fn validate_launch(
    rows: u16,
    cols: u16,
    command: &[String],
    initial_input: Option<&[u8]>,
) -> Result<(), TerminalError> {
    validate_dimensions(rows, cols)?;
    if command.is_empty()
        || command[0].is_empty()
        || command.len() > MAX_ARGS
        || command
            .iter()
            .any(|arg| arg.len() > MAX_ARG_BYTES || arg.as_bytes().contains(&0))
    {
        return Err(TerminalError::InvalidCommand);
    }
    if initial_input
        .is_some_and(|bytes| bytes.len() > MAX_STARTUP_INPUT_BYTES || bytes.contains(&0))
    {
        return Err(TerminalError::Runtime(
            "invalid terminal startup input".into(),
        ));
    }
    Ok(())
}
fn validate_dimensions(rows: u16, cols: u16) -> Result<(), TerminalError> {
    if rows == 0
        || cols == 0
        || rows > MAX_ROWS
        || cols > MAX_COLS
        || usize::from(rows) * usize::from(cols) > MAX_CELLS
    {
        Err(TerminalError::InvalidDimensions { rows, cols })
    } else {
        Ok(())
    }
}
fn modifiers(shift: bool, control: bool, alt: bool, super_key: bool) -> u16 {
    (if shift { ghostty::MOD_SHIFT } else { 0 })
        | (if control { ghostty::MOD_CTRL } else { 0 })
        | (if alt { ghostty::MOD_ALT } else { 0 })
        | (if super_key { ghostty::MOD_SUPER } else { 0 })
}
fn encode_key(emulator: &mut Emulator, key: &KeyEvent) -> Result<Vec<u8>, ghostty::Error> {
    emulator.sync_input();
    encode_synced_key(emulator, key)
}

fn encode_synced_key(emulator: &mut Emulator, key: &KeyEvent) -> Result<Vec<u8>, ghostty::Error> {
    let mut event = ghostty::KeyEvent::new()?;
    event.set_action(if key.repeat {
        ghostty::ffi::GhosttyKeyAction_GHOSTTY_KEY_ACTION_REPEAT
    } else {
        ghostty::ffi::GhosttyKeyAction_GHOSTTY_KEY_ACTION_PRESS
    });
    event.set_mods(modifiers(key.shift, key.control, key.alt, key.super_key));
    event.set_key(ghostty_key(key));
    if !key.text.is_empty() {
        event.set_utf8(&key.text);
        if let Some(character) = key.text.chars().next() {
            event.set_unshifted_codepoint(character.to_ascii_lowercase() as u32);
        }
    }
    emulator.keys.encode(&event)
}

fn ghostty_key(key: &KeyEvent) -> u32 {
    use ghostty::ffi::*;
    match key.code {
        KeyCode::Enter => GhosttyKey_GHOSTTY_KEY_ENTER,
        KeyCode::Backspace => GhosttyKey_GHOSTTY_KEY_BACKSPACE,
        KeyCode::Tab => GhosttyKey_GHOSTTY_KEY_TAB,
        KeyCode::Escape => GhosttyKey_GHOSTTY_KEY_ESCAPE,
        KeyCode::Insert => GhosttyKey_GHOSTTY_KEY_INSERT,
        KeyCode::Delete => GhosttyKey_GHOSTTY_KEY_DELETE,
        KeyCode::Home => GhosttyKey_GHOSTTY_KEY_HOME,
        KeyCode::End => GhosttyKey_GHOSTTY_KEY_END,
        KeyCode::PageUp => GhosttyKey_GHOSTTY_KEY_PAGE_UP,
        KeyCode::PageDown => GhosttyKey_GHOSTTY_KEY_PAGE_DOWN,
        KeyCode::Left => GhosttyKey_GHOSTTY_KEY_ARROW_LEFT,
        KeyCode::Right => GhosttyKey_GHOSTTY_KEY_ARROW_RIGHT,
        KeyCode::Up => GhosttyKey_GHOSTTY_KEY_ARROW_UP,
        KeyCode::Down => GhosttyKey_GHOSTTY_KEY_ARROW_DOWN,
        KeyCode::Function(number) if (1..=35).contains(&number) => {
            GhosttyKey_GHOSTTY_KEY_F1 + u32::from(number - 1)
        }
        KeyCode::Function(_) => 0,
        KeyCode::Text => key
            .text
            .chars()
            .next()
            .map(|ch| match ch.to_ascii_lowercase() {
                'a'..='z' => {
                    GhosttyKey_GHOSTTY_KEY_A + (ch.to_ascii_lowercase() as u32 - 'a' as u32)
                }
                '0'..='9' => GhosttyKey_GHOSTTY_KEY_DIGIT_0 + (ch as u32 - '0' as u32),
                ' ' => GhosttyKey_GHOSTTY_KEY_SPACE,
                _ => 0,
            })
            .unwrap_or(0),
    }
}

#[cfg(test)]
mod tests;
