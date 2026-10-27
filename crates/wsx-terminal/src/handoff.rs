//! Live handoff: the terminal state a successor daemon restores, and the runtime calls around it.
// ^ docs/daemon-live-upgrade.md
use super::*;

pub(super) const HANDOFF_PAUSE_TIMEOUT: Duration = Duration::from_secs(2);

pub const MAX_HANDOFF_ANSI_BYTES: usize = 512 * 1024;

pub(super) const HANDOFF_MODES: &[u16] = &[
    ghostty::MODE_APPLICATION_CURSOR_KEYS,
    ghostty::MODE_FOCUS_EVENT,
    ghostty::MODE_MOUSE_UTF8,
    ghostty::MODE_MOUSE_SGR,
    ghostty::MODE_MOUSE_ALTERNATE_SCROLL,
    ghostty::MODE_MOUSE_SGR_PIXELS,
    ghostty::MODE_BRACKETED_PASTE,
    ghostty::MODE_SYNCHRONIZED_OUTPUT,
    ghostty::MODE_GRAPHEME_CLUSTER,
    ghostty::MODE_COLOR_SCHEME_REPORT,
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalHandoffState {
    pub pane_id: PaneId,
    pub terminal_id: TerminalId,
    pub process_group: libc::pid_t,
    pub rows: u16,
    pub cols: u16,
    pub revision: u64,
    pub history_ansi: String,
    pub history_truncated: bool,
    pub cursor_x: u16,
    pub cursor_y: u16,
    pub modes: Vec<(u16, bool)>,
    pub kitty_keyboard_flags: u8,
    #[serde(default)]
    pub keyboard_state_ansi: String,
    pub alternate_screen: bool,
}

pub(super) fn set_reader_paused(
    control: &ReaderControl,
    paused: bool,
    timeout: Duration,
) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    let mut state = lock(&control.state);
    state.pause_requested = paused;
    if let Some(wake) = lock(&control.wake).as_mut() {
        wake.write_all(&[1])?;
    }
    control.changed.notify_all();
    while state.paused != paused {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                if paused {
                    "timed out pausing PTY reader"
                } else {
                    "timed out resuming PTY reader"
                },
            ));
        }
        let (next, result) = control
            .changed
            .wait_timeout(state, remaining)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state = next;
        if result.timed_out() && state.paused != paused {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "PTY reader state transition timed out",
            ));
        }
    }
    Ok(())
}

/// An emulator with a handed-off screen: the keyboard state, the bounded history, the modes
/// and the cursor.
pub(super) fn restored_emulator(
    state: &TerminalHandoffState,
    writer: Arc<Mutex<Option<Box<dyn Write + Send>>>>,
    error: Arc<Mutex<Option<String>>>,
) -> Result<Emulator, TerminalError> {
    let mut emulator = make_emulator(state.cols, state.rows, writer, error)?;
    if state.alternate_screen {
        emulator.terminal.write(b"\x1b[?1049h");
    }
    // ^ docs/daemon-live-upgrade.md: the keyboard-state dump repeats the visible
    // screen, so apply it first and erase its text before the bounded history.
    emulator
        .terminal
        .write(state.keyboard_state_ansi.as_bytes());
    emulator.terminal.write(b"\x1b[0m\x1b[H\x1b[2J\x1b[3J");
    emulator.terminal.write(state.history_ansi.as_bytes());
    for (mode, value) in &state.modes {
        emulator.terminal.mode_set(*mode, *value)?;
    }
    let cursor = format!("\x1b[{};{}H", state.cursor_y + 1, state.cursor_x + 1);
    emulator.terminal.write(cursor.as_bytes());
    emulator.sync_input();
    Ok(emulator)
}

pub(super) fn bounded_handoff_ansi(
    terminal: &ghostty::Terminal,
    cols: u16,
    total_rows: usize,
    max_bytes: usize,
) -> Result<(String, bool), ghostty::Error> {
    let end_row = u32::try_from(total_rows.saturating_sub(1)).unwrap_or(u32::MAX);
    let end_col = cols.saturating_sub(1);
    let mut ansi = terminal.read_ansi_screen((0, 0), (end_col, end_row), false, false)?;
    if ansi.len() <= max_bytes {
        return Ok((ansi, false));
    }
    let mut low = 0usize;
    let mut high = total_rows;
    while low < high {
        let middle = low + (high - low) / 2;
        let middle_row = u32::try_from(middle).unwrap_or(u32::MAX);
        let candidate =
            terminal.read_ansi_screen((0, middle_row), (end_col, end_row), false, false)?;
        if candidate.len() > max_bytes {
            low = middle.saturating_add(1);
        } else {
            high = middle;
            ansi = candidate;
        }
    }
    if ansi.len() > max_bytes {
        ansi.clear();
    }
    Ok((ansi, true))
}

impl TerminalRuntime {
    pub fn pause_handoff_reader(&self) -> Result<(), TerminalError> {
        set_reader_paused(&self.shared.reader, true, HANDOFF_PAUSE_TIMEOUT)
            .map_err(TerminalError::Io)
    }

    pub fn resume_handoff_reader(&self) {
        let _ = set_reader_paused(&self.shared.reader, false, HANDOFF_PAUSE_TIMEOUT);
    }

    pub fn duplicate_handoff_fd(&self) -> Result<RawFd, TerminalError> {
        let process = self
            .process
            .as_ref()
            .ok_or_else(|| TerminalError::Runtime("terminal has no PTY".into()))?;
        let master = lock(&process.master);
        duplicate_fd(
            master
                .as_raw_fd()
                .ok_or_else(|| TerminalError::Runtime("PTY has no file descriptor".into()))?,
        )
        .map(|fd| fd.into_raw_fd())
        .map_err(TerminalError::Io)
    }

    /// The terminal's screen, history and modes for a live handoff. An exited terminal has
    /// no process group to hand over: its state carries 0 and goes without a PTY
    /// (docs/daemon-live-upgrade.md "Exited panes").
    pub fn handoff_state(&self) -> Result<TerminalHandoffState, TerminalError> {
        let process_group = match self.process_group_id() {
            Some(group) => group,
            None if self.exited() => 0,
            None => {
                return Err(TerminalError::Runtime(
                    "terminal process group is unavailable".into(),
                ))
            }
        };
        let revision = self.shared.revision.load(Ordering::Acquire);
        let mut emulator = lock(&self.shared.emulator);
        let Emulator {
            terminal, render, ..
        } = &mut *emulator;
        render.update(terminal)?;
        let cols = terminal.cols()?;
        let rows = terminal.rows()?;
        let total_rows = terminal.total_rows()?.max(1);
        let (history_ansi, history_truncated) =
            bounded_handoff_ansi(terminal, cols, total_rows, MAX_HANDOFF_ANSI_BYTES)?;
        let (cursor_x, cursor_y) = render
            .cursor_viewport()?
            .map_or((0, 0), |cursor| (cursor.x, cursor.y));
        let modes = HANDOFF_MODES
            .iter()
            .map(|mode| terminal.mode_get(*mode).map(|value| (*mode, value)))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(TerminalHandoffState {
            pane_id: self.pane_id,
            terminal_id: self.terminal_id,
            process_group,
            rows,
            cols,
            revision,
            history_ansi,
            history_truncated,
            cursor_x,
            cursor_y,
            modes,
            kitty_keyboard_flags: terminal.kitty_keyboard_flags()?,
            keyboard_state_ansi: terminal.keyboard_state_ansi()?,
            alternate_screen: terminal.active_screen()? == ghostty::ActiveScreen::Alternate,
        })
    }

    /// Reconstructs a quiesced runtime around an owned PTY master descriptor.
    ///
    /// # Safety
    ///
    /// `fd` must be a valid, uniquely owned PTY master descriptor for the
    /// process group recorded in `state`. This function assumes ownership.
    pub unsafe fn from_handoff_fd(
        fd: RawFd,
        state: TerminalHandoffState,
        notify: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Self, TerminalError> {
        validate_dimensions(state.rows, state.cols)?;
        if fd < 0 || state.process_group <= 1 {
            return Err(TerminalError::Runtime(
                "invalid terminal handoff state".into(),
            ));
        }
        let master = portable_pty::unix::master_from_raw_fd(fd)
            .map_err(|error| TerminalError::Pty(error.to_string()))?;
        let reader_fd = duplicate_fd(
            master
                .as_raw_fd()
                .ok_or_else(|| TerminalError::Runtime("PTY has no file descriptor".into()))?,
        )?;
        let writer = master
            .take_writer()
            .map_err(|error| TerminalError::Pty(error.to_string()))?;
        let writer = Arc::new(Mutex::new(Some(writer)));
        let error = Arc::new(Mutex::new(None));
        let emulator = restored_emulator(&state, Arc::clone(&writer), Arc::clone(&error))?;
        let (reader, reader_wake) = reader_control()?;
        {
            let mut reader_state = lock(&reader.state);
            reader_state.pause_requested = true;
        }
        let shared = Arc::new(Shared {
            emulator: Mutex::new(emulator),
            writer,
            error,
            revision: AtomicU64::new(state.revision.max(1)),
            exited: AtomicBool::new(false),
            terminating: AtomicBool::new(false),
            notify,
            reader,
        });
        spawn_reader_fd(reader_fd, reader_wake, Arc::clone(&shared))?;
        set_reader_paused(&shared.reader, true, HANDOFF_PAUSE_TIMEOUT)
            .map_err(TerminalError::Io)?;
        Ok(Self {
            pane_id: state.pane_id,
            terminal_id: state.terminal_id,
            shared,
            process: Some(Process {
                master: Mutex::new(master),
                killer: Mutex::new(None),
                process_group: Arc::new(std::sync::atomic::AtomicI32::new(state.process_group)),
            }),
            selection_clock: Instant::now(),
            preserve_process_on_drop: AtomicBool::new(false),
        })
    }

    /// An exited terminal from a live handoff: its last screen and history, no PTY and no
    /// process. Input goes nowhere; it reads as exited (docs/daemon-live-upgrade.md
    /// "Exited panes").
    pub fn exited_from_handoff(
        state: TerminalHandoffState,
        notify: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Self, TerminalError> {
        validate_dimensions(state.rows, state.cols)?;
        let writer: Arc<Mutex<Option<Box<dyn Write + Send>>>> =
            Arc::new(Mutex::new(Some(Box::new(io::sink()))));
        let error = Arc::new(Mutex::new(None));
        let emulator = restored_emulator(&state, Arc::clone(&writer), Arc::clone(&error))?;
        Ok(Self {
            pane_id: state.pane_id,
            terminal_id: state.terminal_id,
            shared: Arc::new(Shared {
                emulator: Mutex::new(emulator),
                writer,
                error,
                revision: AtomicU64::new(state.revision.max(1)),
                exited: AtomicBool::new(true),
                terminating: AtomicBool::new(false),
                notify,
                reader: Arc::new(ReaderControl::default()),
            }),
            process: None,
            selection_clock: Instant::now(),
            preserve_process_on_drop: AtomicBool::new(false),
        })
    }

    pub fn activate_handoff(&self) {
        self.resume_handoff_reader();
        if let Some(process_group) = self.process_group_id() {
            unsafe {
                libc::kill(-process_group, libc::SIGWINCH);
            }
        }
        self.shared.revision.fetch_add(1, Ordering::AcqRel);
        (self.shared.notify)();
    }

    pub fn protect_handoff_import(&self) {
        self.preserve_process_on_drop.store(true, Ordering::Release);
    }

    pub fn prepare_source_handoff(&self) -> Result<libc::pid_t, TerminalError> {
        let process = self
            .process
            .as_ref()
            .ok_or_else(|| TerminalError::Runtime("terminal has no PTY".into()))?;
        let process_group = process.process_group.swap(0, Ordering::AcqRel);
        if process_group <= 1 {
            return Err(TerminalError::Runtime(
                "terminal process ownership is unavailable".into(),
            ));
        }
        self.protect_handoff_import();
        Ok(process_group)
    }

    pub fn rollback_source_handoff(&self, process_group: libc::pid_t) {
        if let Some(process) = &self.process {
            let _ = process.process_group.compare_exchange(
                0,
                process_group,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
        }
        self.preserve_process_on_drop
            .store(false, Ordering::Release);
    }

    pub fn assume_handoff_ownership(&self) {
        self.preserve_process_on_drop
            .store(false, Ordering::Release);
    }
}
