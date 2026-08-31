//! Persistent PTY runtime backed by pinned libghostty-vt.
// ^ [[wsx Architecture]] The daemon owns this type; clients only receive frames.

mod ghostty;

use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use std::{
    error::Error,
    fmt,
    io::{self, Read, Write},
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex, MutexGuard,
    },
    thread,
    time::{Duration, Instant},
};
use wsx_core::runtime::{
    Cell, CellModifiers, Cursor, KeyCode, KeyEvent, MouseAction, MouseButton, MouseEvent, PaneId,
    TerminalFrame, TerminalId, TerminalRowPatch, TerminalUpdate,
};

const MAX_SCROLLBACK_ROWS: usize = 10_000;
const MAX_ROWS: u16 = 1_000;
const MAX_COLS: u16 = 1_000;
const MAX_CELLS: usize = 100_000;
const MAX_ARGS: usize = 256;
const MAX_ARG_BYTES: usize = 64 * 1024;
const MAX_STARTUP_INPUT_BYTES: usize = 64 * 1024;
const STARTUP_WRITE_TIMEOUT: Duration = Duration::from_secs(2);
const BRACKETED_PASTE_MODE: u16 = 2004;

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
}
impl Emulator {
    fn sync_input(&mut self) {
        self.keys.set_from_terminal(&self.terminal);
        self.mouse.set_from_terminal(&self.terminal);
    }
}

struct Process {
    master: Mutex<Box<dyn MasterPty + Send>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    #[cfg(unix)]
    process_group: Arc<std::sync::atomic::AtomicI32>,
}
struct Shared {
    emulator: Mutex<Emulator>,
    writer: Arc<Mutex<Option<Box<dyn Write + Send>>>>,
    error: Arc<Mutex<Option<String>>>,
    revision: AtomicU64,
    exited: AtomicBool,
    terminating: AtomicBool,
    notify: Arc<dyn Fn() + Send + Sync>,
}

pub struct TerminalRuntime {
    pane_id: PaneId,
    terminal_id: TerminalId,
    shared: Arc<Shared>,
    process: Option<Process>,
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
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| TerminalError::Pty(e.to_string()))?;
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
        for (name, value) in environment {
            builder.env(name, value);
        }
        let child = pair
            .slave
            .spawn_command(builder)
            .map_err(|e| TerminalError::Pty(e.to_string()))?;
        let mut killer = child.clone_killer();
        #[cfg(unix)]
        let process_group = pair.master.process_group_leader();
        #[cfg(unix)]
        let process_group_owner = Arc::new(std::sync::atomic::AtomicI32::new(
            process_group.unwrap_or_default(),
        ));
        if let Some(bytes) = initial_input.filter(|bytes| !bytes.is_empty()) {
            if let Err(error) = write_startup_input(pair.master.as_ref(), bytes) {
                abort_spawn(
                    &mut killer,
                    #[cfg(unix)]
                    process_group,
                );
                return Err(TerminalError::Io(error));
            }
        }
        let shared = Arc::new(Shared {
            emulator: Mutex::new(emulator),
            writer,
            error,
            revision: AtomicU64::new(1),
            exited: AtomicBool::new(false),
            terminating: AtomicBool::new(false),
            notify,
        });
        if let Err(error) = spawn_reader(reader, Arc::clone(&shared)) {
            abort_spawn(
                &mut killer,
                #[cfg(unix)]
                process_group,
            );
            return Err(error);
        }
        if let Err(error) = spawn_waiter(
            child,
            Arc::clone(&shared),
            #[cfg(unix)]
            Arc::clone(&process_group_owner),
        ) {
            abort_spawn(
                &mut killer,
                #[cfg(unix)]
                process_group,
            );
            return Err(error);
        }
        Ok(Self {
            pane_id,
            terminal_id,
            shared,
            process: Some(Process {
                master: Mutex::new(pair.master),
                killer: Mutex::new(killer),
                #[cfg(unix)]
                process_group: process_group_owner,
            }),
        })
    }

    #[cfg(unix)]
    pub fn process_group_id(&self) -> Option<libc::pid_t> {
        self.process.as_ref().and_then(|process| {
            let group = process.process_group.load(Ordering::Acquire);
            (group > 1).then_some(group)
        })
    }

    pub fn write(&self, bytes: &[u8]) -> Result<(), TerminalError> {
        if bytes.is_empty() {
            return Ok(());
        }
        if self.exited() {
            return Err(TerminalError::Runtime("terminal process exited".into()));
        }
        write_pty(&self.shared.writer, bytes).map_err(TerminalError::Io)
    }

    pub fn key(&self, key: &KeyEvent) -> Result<(), TerminalError> {
        if key.text.len() > 16 {
            return Err(TerminalError::Runtime("key text exceeds limit".into()));
        }
        let bytes = {
            let mut emulator = lock(&self.shared.emulator);
            emulator.sync_input();
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
            emulator.keys.encode(&event)?
        };
        self.write(&bytes)
    }

    pub fn paste(&self, text: &str) -> Result<(), TerminalError> {
        let bytes = {
            let emulator = lock(&self.shared.emulator);
            ghostty::encode_paste(
                text.as_bytes(),
                emulator.terminal.mode_get(BRACKETED_PASTE_MODE)?,
            )?
        };
        self.write(&bytes)
    }

    pub fn mouse(&self, mouse: &MouseEvent) -> Result<(), TerminalError> {
        let bytes = {
            let mut emulator = lock(&self.shared.emulator);
            emulator.sync_input();
            let mut event = ghostty::MouseEvent::new()?;
            event.set_action(match mouse.action {
                MouseAction::Press => ghostty::MOUSE_ACTION_PRESS,
                MouseAction::Release => ghostty::MOUSE_ACTION_RELEASE,
                MouseAction::Motion => ghostty::MOUSE_ACTION_MOTION,
            });
            match mouse.button {
                MouseButton::Left => event.set_button(ghostty::MOUSE_BUTTON_LEFT),
                MouseButton::Middle => event.set_button(ghostty::MOUSE_BUTTON_MIDDLE),
                MouseButton::Right => event.set_button(ghostty::MOUSE_BUTTON_RIGHT),
                MouseButton::WheelUp => event.set_button(ghostty::MOUSE_BUTTON_WHEEL_UP),
                MouseButton::WheelDown => event.set_button(ghostty::MOUSE_BUTTON_WHEEL_DOWN),
                MouseButton::WheelLeft => event.set_button(ghostty::MOUSE_BUTTON_WHEEL_LEFT),
                MouseButton::WheelRight => event.set_button(ghostty::MOUSE_BUTTON_WHEEL_RIGHT),
                MouseButton::None => event.clear_button(),
            }
            event.set_mods(modifiers(
                mouse.shift,
                mouse.control,
                mouse.alt,
                mouse.super_key,
            ));
            event.set_position(f32::from(mouse.x), f32::from(mouse.y));
            emulator.mouse.encode(&event)?
        };
        self.write(&bytes)
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
        emulator
            .mouse
            .set_size(u32::from(cols), u32::from(rows), 1, 1);
        emulator.sync_input();
        self.shared.revision.fetch_add(1, Ordering::AcqRel);
        drop(emulator);
        (self.shared.notify)();
        Ok(())
    }

    pub fn frame(&self) -> Result<TerminalFrame, TerminalError> {
        if let Some(error) = lock(&self.shared.error).clone() {
            return Err(TerminalError::Runtime(error));
        }
        let mut emulator = lock(&self.shared.emulator);
        let Emulator {
            terminal, render, ..
        } = &mut *emulator;
        render.update(terminal)?;
        let cols = emulator.render.cols()?;
        let rows = emulator.render.rows()?;
        validate_dimensions(rows, cols)?;
        let colors = emulator.render.colors()?;
        let mut cells = Vec::with_capacity(usize::from(rows) * usize::from(cols));
        let mut rows_handle = ghostty::RowIterator::new()?;
        let mut cells_handle = ghostty::RowCells::new()?;
        let mut rows_iter = emulator.render.populate_row_iterator(&mut rows_handle)?;
        let mut bytes = Vec::new();
        let mut symbol = String::new();
        for _ in 0..rows {
            if !rows_iter.next() {
                cells.extend((0..cols).map(|_| blank(colors.foreground)));
                continue;
            }
            let mut iter = rows_iter.populate_cells(&mut cells_handle)?;
            for _ in 0..cols {
                if !iter.next() {
                    cells.push(blank(colors.foreground));
                    continue;
                }
                iter.grapheme_text_into(&mut bytes, &mut symbol)?;
                let basic = iter.basic_data()?;
                let fg = iter.fg_color()?.unwrap_or(colors.foreground);
                let bg = iter.bg_color()?;
                let width = cell_width(basic.wide);
                normalize_cell_symbol(&mut symbol, width);
                cells.push(Cell {
                    symbol: symbol.clone(),
                    fg: Some([fg.r, fg.g, fg.b]),
                    bg: bg.map(|color| [color.r, color.g, color.b]),
                    modifiers: CellModifiers {
                        bold: basic.style.bold,
                        italic: basic.style.italic,
                        underline: basic.style.underlined,
                        inverse: basic.style.inverse,
                        dim: basic.style.faint,
                        strike: basic.style.strikethrough,
                    },
                    width,
                });
            }
        }
        let viewport = emulator.render.cursor_viewport()?;
        let style = emulator.render.cursor_visual_style()?;
        Ok(TerminalFrame {
            pane_id: self.pane_id,
            terminal_id: self.terminal_id,
            revision: self.shared.revision.load(Ordering::Acquire),
            cols,
            rows,
            cells,
            cursor: Cursor {
                x: viewport.map_or(0, |cursor| cursor.x),
                y: viewport.map_or(0, |cursor| cursor.y),
                visible: emulator.render.cursor_visible()? && viewport.is_some(),
                blinking: emulator.render.cursor_blinking()?,
                shape: match style {
                    ghostty::CursorVisualStyle::Block => 0,
                    ghostty::CursorVisualStyle::Underline => 1,
                    ghostty::CursorVisualStyle::Bar => 2,
                    ghostty::CursorVisualStyle::BlockHollow => 3,
                },
            },
        })
    }

    pub fn frame_update(
        &self,
        base_revision: Option<u64>,
    ) -> Result<TerminalUpdate, TerminalError> {
        if let Some(error) = lock(&self.shared.error).clone() {
            return Err(TerminalError::Runtime(error));
        }
        let mut emulator = lock(&self.shared.emulator);
        let revision = self.shared.revision.load(Ordering::Acquire);
        let Emulator {
            terminal, render, ..
        } = &mut *emulator;
        render.update(terminal)?;
        let cols = render.cols()?;
        let rows = render.rows()?;
        validate_dimensions(rows, cols)?;
        let full = base_revision.is_none() || render.dirty()? == ghostty::Dirty::Full;
        let colors = render.colors()?;
        let mut rows_handle = ghostty::RowIterator::new()?;
        let mut cells_handle = ghostty::RowCells::new()?;
        let mut rows_iter = render.populate_row_iterator(&mut rows_handle)?;
        let mut all_cells = Vec::with_capacity(usize::from(rows) * usize::from(cols));
        let mut changed_rows = Vec::new();
        let mut bytes = Vec::new();
        let mut symbol = String::new();
        for row in 0..rows {
            if !rows_iter.next() {
                if full {
                    all_cells.extend((0..cols).map(|_| blank(colors.foreground)));
                }
                continue;
            }
            let row_dirty = rows_iter.dirty()?;
            if !full && !row_dirty {
                continue;
            }
            let mut row_cells = Vec::with_capacity(usize::from(cols));
            let mut iter = rows_iter.populate_cells(&mut cells_handle)?;
            for _ in 0..cols {
                if !iter.next() {
                    row_cells.push(blank(colors.foreground));
                    continue;
                }
                iter.grapheme_text_into(&mut bytes, &mut symbol)?;
                let basic = iter.basic_data()?;
                let fg = iter.fg_color()?.unwrap_or(colors.foreground);
                let bg = iter.bg_color()?;
                let width = cell_width(basic.wide);
                normalize_cell_symbol(&mut symbol, width);
                row_cells.push(Cell {
                    symbol: symbol.clone(),
                    fg: Some([fg.r, fg.g, fg.b]),
                    bg: bg.map(|color| [color.r, color.g, color.b]),
                    modifiers: CellModifiers {
                        bold: basic.style.bold,
                        italic: basic.style.italic,
                        underline: basic.style.underlined,
                        inverse: basic.style.inverse,
                        dim: basic.style.faint,
                        strike: basic.style.strikethrough,
                    },
                    width,
                });
            }
            rows_iter.clear_dirty()?;
            if full {
                all_cells.extend(row_cells);
            } else {
                changed_rows.push(TerminalRowPatch {
                    row,
                    cells: row_cells,
                });
            }
        }
        render.set_dirty(ghostty::Dirty::Clean)?;
        let viewport = render.cursor_viewport()?;
        let style = render.cursor_visual_style()?;
        let cursor = Cursor {
            x: viewport.map_or(0, |cursor| cursor.x),
            y: viewport.map_or(0, |cursor| cursor.y),
            visible: render.cursor_visible()? && viewport.is_some(),
            blinking: render.cursor_blinking()?,
            shape: match style {
                ghostty::CursorVisualStyle::Block => 0,
                ghostty::CursorVisualStyle::Underline => 1,
                ghostty::CursorVisualStyle::Bar => 2,
                ghostty::CursorVisualStyle::BlockHollow => 3,
            },
        };
        if full {
            Ok(TerminalUpdate::Full(TerminalFrame {
                pane_id: self.pane_id,
                terminal_id: self.terminal_id,
                revision,
                cols,
                rows,
                cells: all_cells,
                cursor,
            }))
        } else {
            Ok(TerminalUpdate::Patch {
                pane_id: self.pane_id,
                terminal_id: self.terminal_id,
                base_revision: base_revision.unwrap_or_default(),
                revision,
                cols,
                rows,
                changed_rows,
                cursor,
            })
        }
    }

    pub fn revision(&self) -> u64 {
        self.shared.revision.load(Ordering::Acquire)
    }

    pub fn synchronized_output_active(&self) -> bool {
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
        lock(&self.shared.writer).take();
        if let Some(process) = &self.process {
            #[cfg(unix)]
            terminate_group(take_process_group(&process.process_group));
            let _ = lock(&process.killer).kill();
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
            shared: Arc::new(Shared {
                emulator: Mutex::new(emulator),
                writer,
                error,
                revision: AtomicU64::new(1),
                exited: AtomicBool::new(false),
                terminating: AtomicBool::new(false),
                notify: Arc::new(|| {}),
            }),
            process: None,
        })
    }
}
impl Drop for TerminalRuntime {
    fn drop(&mut self) {
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
    terminal.set_write_pty_callback(move |bytes| {
        if let Err(write_error) = write_pty(&writer, bytes) {
            set_error(
                &error,
                format!("terminal query response failed: {write_error}"),
            );
        }
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
    })
}

fn spawn_reader(
    mut reader: Box<dyn Read + Send>,
    shared: Arc<Shared>,
) -> Result<(), TerminalError> {
    thread::Builder::new()
        .name("wsx-pty-reader".into())
        .spawn(move || {
            let mut buffer = [0_u8; 16 * 1024];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => {
                        let mut emulator = lock(&shared.emulator);
                        emulator.terminal.write(&buffer[..count]);
                        emulator.sync_input();
                        shared.revision.fetch_add(1, Ordering::AcqRel);
                        drop(emulator);
                        (shared.notify)();
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error)
                        if error.kind() == io::ErrorKind::UnexpectedEof
                            || error.raw_os_error() == Some(libc::EIO) =>
                    {
                        break
                    }
                    Err(error) => {
                        set_error(&shared.error, format!("PTY reader failed: {error}"));
                        break;
                    }
                }
            }
            mark_exited(&shared);
        })
        .map(|_| ())
        .map_err(TerminalError::Io)
}

fn spawn_waiter(
    mut child: Box<dyn portable_pty::Child + Send + Sync>,
    shared: Arc<Shared>,
    #[cfg(unix)] process_group: Arc<std::sync::atomic::AtomicI32>,
) -> Result<(), TerminalError> {
    thread::Builder::new()
        .name("wsx-child-waiter".into())
        .spawn(move || {
            if let Err(error) = child.wait() {
                set_error(&shared.error, format!("child wait failed: {error}"));
            }
            #[cfg(unix)]
            terminate_group(take_process_group(&process_group));
            mark_exited(&shared);
        })
        .map(|_| ())
        .map_err(TerminalError::Io)
}

fn abort_spawn(
    killer: &mut Box<dyn ChildKiller + Send + Sync>,
    #[cfg(unix)] process_group: Option<libc::pid_t>,
) {
    #[cfg(unix)]
    terminate_group(process_group);
    let _ = killer.kill();
}

#[cfg(unix)]
fn take_process_group(group: &std::sync::atomic::AtomicI32) -> Option<libc::pid_t> {
    let group = group.swap(0, Ordering::AcqRel);
    (group > 1).then_some(group)
}

#[cfg(unix)]
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

#[cfg(unix)]
fn write_startup_input(master: &dyn MasterPty, bytes: &[u8]) -> io::Result<()> {
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
    let deadline = Instant::now() + STARTUP_WRITE_TIMEOUT;
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
                "timed out writing terminal startup input",
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

#[cfg(not(unix))]
fn write_startup_input(_master: &dyn MasterPty, _bytes: &[u8]) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "terminal startup input is unsupported on this platform",
    ))
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
    if !shared.exited.swap(true, Ordering::AcqRel) {
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
fn cell_width(width: ghostty::CellWide) -> wsx_core::runtime::CellWidth {
    match width {
        ghostty::CellWide::Narrow => wsx_core::runtime::CellWidth::Narrow,
        ghostty::CellWide::Wide => wsx_core::runtime::CellWidth::Wide,
        ghostty::CellWide::SpacerHead => wsx_core::runtime::CellWidth::SpacerHead,
        ghostty::CellWide::SpacerTail => wsx_core::runtime::CellWidth::SpacerTail,
    }
}

fn normalize_cell_symbol(symbol: &mut String, width: wsx_core::runtime::CellWidth) {
    match width {
        wsx_core::runtime::CellWidth::SpacerTail => symbol.clear(),
        wsx_core::runtime::CellWidth::Wide if symbol.is_empty() => symbol.push_str("  "),
        wsx_core::runtime::CellWidth::Narrow | wsx_core::runtime::CellWidth::SpacerHead
            if symbol.is_empty() =>
        {
            symbol.push(' ')
        }
        _ => {}
    }
}

fn blank(fg: ghostty::RgbColor) -> Cell {
    Cell {
        symbol: String::new(),
        fg: Some([fg.r, fg.g, fg.b]),
        bg: None,
        modifiers: CellModifiers::default(),
        width: wsx_core::runtime::CellWidth::Narrow,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_unbounded_dimensions_and_commands() {
        assert!(validate_launch(0, 80, &["sh".into()], None).is_err());
        assert!(validate_launch(1_000, 1_000, &["sh".into()], None).is_err());
        assert!(validate_launch(24, 80, &["sh\0bad".into()], None).is_err());
        assert!(validate_launch(24, 80, &["sh".into()], Some(&vec![b'x'; 64 * 1024 + 1])).is_err());
    }
    #[test]
    fn frame_is_complete_and_styled() {
        let runtime = TerminalRuntime::new_for_test(2, 4).unwrap();
        {
            let mut emulator = lock(&runtime.shared.emulator);
            emulator.terminal.write(b"\x1b[31;1mA\x1b[0mB\x1b[2;3H");
        }
        let frame = runtime.frame().unwrap();
        assert_eq!((frame.rows, frame.cols, frame.cells.len()), (2, 4, 8));
        assert_eq!(frame.cells[0].symbol, "A");
        assert!(frame.cells[0].modifiers.bold);
        assert!(frame.cells.iter().all(|cell| cell.bg.is_none()));
        assert_eq!((frame.cursor.x, frame.cursor.y), (2, 1));
    }
    #[test]
    fn frame_keeps_only_explicit_cell_backgrounds() {
        let runtime = TerminalRuntime::new_for_test(1, 3).unwrap();
        {
            let mut emulator = lock(&runtime.shared.emulator);
            emulator.terminal.write(b"\x1b[44mA\x1b[0mB");
        }
        let frame = runtime.frame().unwrap();
        assert!(frame.cells[0].bg.is_some());
        assert_eq!(frame.cells[1].bg, None);
        assert_eq!(frame.cells[2].bg, None);
    }
    #[test]
    fn frame_update_uses_dirty_rows_after_the_full_baseline() {
        let runtime = TerminalRuntime::new_for_test(2, 4).unwrap();
        let initial = runtime.frame_update(None).unwrap();
        let base_revision = initial.revision();
        assert!(matches!(initial, TerminalUpdate::Full(_)));
        {
            let mut emulator = lock(&runtime.shared.emulator);
            emulator.terminal.write(b"A");
            emulator.sync_input();
        }
        runtime.shared.revision.fetch_add(1, Ordering::AcqRel);
        let patch = runtime.frame_update(Some(base_revision)).unwrap();
        match patch {
            TerminalUpdate::Patch {
                base_revision: base,
                changed_rows,
                ..
            } => {
                assert_eq!(base, base_revision);
                assert_eq!(changed_rows.len(), 1);
                assert_eq!(changed_rows[0].row, 0);
                assert_eq!(changed_rows[0].cells[0].symbol, "A");
            }
            TerminalUpdate::Full(_) => panic!("expected a dirty-row patch"),
        }
    }

    #[test]
    fn backspace_erase_sequence_updates_one_stable_row() {
        let runtime = TerminalRuntime::new_for_test(1, 8).unwrap();
        {
            let mut emulator = lock(&runtime.shared.emulator);
            emulator.terminal.write(b"abcdef");
            emulator.sync_input();
        }
        runtime.shared.revision.fetch_add(1, Ordering::AcqRel);
        let mut baseline = match runtime.frame_update(None).unwrap() {
            TerminalUpdate::Full(frame) => Some(frame),
            TerminalUpdate::Patch { .. } => panic!("expected full frame"),
        };
        let base = baseline.as_ref().unwrap().revision;

        {
            let mut emulator = lock(&runtime.shared.emulator);
            emulator.terminal.write(b"\x08 \x08");
            emulator.sync_input();
        }
        runtime.shared.revision.fetch_add(1, Ordering::AcqRel);
        runtime
            .frame_update(Some(base))
            .unwrap()
            .apply_to(&mut baseline)
            .unwrap();

        let frame = baseline.unwrap();
        let text = frame
            .cells
            .iter()
            .map(|cell| cell.symbol.as_str())
            .collect::<String>();
        assert_eq!(text, "abcde   ");
        assert_eq!((frame.cursor.x, frame.cursor.y), (5, 0));
    }

    #[test]
    fn synchronized_output_mode_is_authoritative() {
        let runtime = TerminalRuntime::new_for_test(2, 4).unwrap();
        {
            let mut emulator = lock(&runtime.shared.emulator);
            emulator.terminal.write(b"\x1b[?2026h");
        }
        assert!(runtime.synchronized_output_active());
        {
            let mut emulator = lock(&runtime.shared.emulator);
            emulator.terminal.write(b"\x1b[?2026l");
        }
        assert!(!runtime.synchronized_output_active());
    }

    #[test]
    fn wide_cells_and_spacer_tails_survive_frame_updates() {
        let runtime = TerminalRuntime::new_for_test(1, 4).unwrap();
        {
            let mut emulator = lock(&runtime.shared.emulator);
            emulator.terminal.write("界".as_bytes());
            emulator.sync_input();
        }
        runtime.shared.revision.fetch_add(1, Ordering::AcqRel);
        let mut frame = match runtime.frame_update(None).unwrap() {
            TerminalUpdate::Full(frame) => frame,
            TerminalUpdate::Patch { .. } => panic!("expected full frame"),
        };
        assert_eq!(frame.cells[0].width, wsx_core::runtime::CellWidth::Wide);
        assert_eq!(
            frame.cells[1].width,
            wsx_core::runtime::CellWidth::SpacerTail
        );

        let base = frame.revision;
        {
            let mut emulator = lock(&runtime.shared.emulator);
            emulator.terminal.write(b"\rA\x1b[K");
            emulator.sync_input();
        }
        runtime.shared.revision.fetch_add(1, Ordering::AcqRel);
        let mut baseline = Some(frame);
        runtime
            .frame_update(Some(base))
            .unwrap()
            .apply_to(&mut baseline)
            .unwrap();
        frame = baseline.unwrap();
        assert_eq!(frame.cells[0].symbol, "A");
        assert_eq!(frame.cells[0].width, wsx_core::runtime::CellWidth::Narrow);
        assert_eq!(frame.cells[1].width, wsx_core::runtime::CellWidth::Narrow);
    }

    #[cfg(unix)]
    #[test]
    fn spawned_process_receives_bounded_runtime_environment() {
        let command = vec![
            "/bin/sh".into(),
            "-c".into(),
            "printf %s \"$WSX_TEST_MARKER\"; sleep 1".into(),
        ];
        let runtime = TerminalRuntime::spawn(
            PaneId(1),
            TerminalId(2),
            &std::env::current_dir().unwrap(),
            &command,
            &[("WSX_TEST_MARKER".into(), "pane-42".into())],
            None,
            2,
            40,
            Arc::new(|| {}),
        )
        .unwrap();
        let mut observed = false;
        for _ in 0..50 {
            let text = runtime
                .frame()
                .unwrap()
                .cells
                .iter()
                .map(|cell| cell.symbol.as_str())
                .collect::<String>();
            if text.contains("pane-42") {
                observed = true;
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        runtime.terminate();
        assert!(observed);
    }

    #[cfg(unix)]
    #[test]
    fn process_group_ownership_is_consumed_once() {
        let group = std::sync::atomic::AtomicI32::new(42);
        assert_eq!(take_process_group(&group), Some(42));
        assert_eq!(take_process_group(&group), None);
    }

    #[test]
    fn paste_encoding_tracks_bracketed_mode_and_sanitizes_controls() {
        assert_eq!(
            ghostty::encode_paste(b"one\ntwo", false).unwrap(),
            b"one\rtwo"
        );
        assert_eq!(
            ghostty::encode_paste(b"one\ntwo", true).unwrap(),
            b"\x1b[200~one\ntwo\x1b[201~"
        );
        assert_eq!(ghostty::encode_paste(b"a\0b", false).unwrap(), b"a b");
    }

    #[test]
    fn runtime_is_send_and_sync() {
        fn assert_traits<T: Send + Sync>() {}
        assert_traits::<TerminalRuntime>();
    }
}
