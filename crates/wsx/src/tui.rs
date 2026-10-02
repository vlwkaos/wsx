// Terminal init/restore wrapper
// ref: ratatui docs — https://ratatui.rs/concepts/backends/

use anyhow::Result;
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use crossterm::{
    cursor::SetCursorStyle,
    event::{DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture},
    execute,
    terminal::{
        disable_raw_mode, enable_raw_mode, BeginSynchronizedUpdate, EndSynchronizedUpdate,
        EnterAlternateScreen, LeaveAlternateScreen,
    },
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::{
    fs::File,
    io::{self, IsTerminal, Stdout, Write},
    marker::PhantomData,
    os::fd::{AsFd, AsRawFd},
    rc::Rc,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};
use wsx_core::runtime::Cursor;

pub type Tui = Terminal<CrosstermBackend<Stdout>>;

type PanicHook = dyn Fn(&std::panic::PanicHookInfo<'_>) + Send + Sync + 'static;

// ^ docs/ui-state-ownership.md: retain bounded ownership evidence, never terminal content.
#[track_caller]
fn record_mode_event(event: &'static str, error: Option<&io::Error>) {
    record_mode_event_at(event, error, std::panic::Location::caller());
}

fn record_mode_event_at(
    event: &'static str,
    error: Option<&io::Error>,
    source: &std::panic::Location<'_>,
) {
    let Some(cache) = dirs::cache_dir() else {
        return;
    };
    let _ = append_mode_event_at(
        &cache.join("wsx/ui-terminal-diagnostics-v1.jsonl"),
        event,
        error,
        source,
    );
}

#[cfg(test)]
#[track_caller]
fn append_mode_event(
    path: &std::path::Path,
    event: &'static str,
    error: Option<&io::Error>,
) -> io::Result<()> {
    append_mode_event_at(path, event, error, std::panic::Location::caller())
}

fn append_mode_event_at(
    path: &std::path::Path,
    event: &'static str,
    error: Option<&io::Error>,
    source: &std::panic::Location<'_>,
) -> io::Result<()> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("diagnostics have no parent"))?;
    std::fs::create_dir_all(parent)?;
    let parent_metadata = std::fs::symlink_metadata(parent)?;
    if !parent_metadata.is_dir()
        || parent_metadata.uid() != unsafe { libc::geteuid() }
        || parent_metadata.mode() & 0o022 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe terminal diagnostics directory",
        ));
    }
    let mut file = File::options()
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.nlink() != 1
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe terminal diagnostics file",
        ));
    }
    // Best-effort evidence cannot block terminal restoration or contend on a live UI lock.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    // Observe the same stdin-or-controlling-terminal selection as the mode owner.
    let flags = TerminalAttributes::capture().ok().map(|attributes| {
        let attributes = attributes.saved;
        [
            attributes.c_iflag,
            attributes.c_oflag,
            attributes.c_cflag,
            attributes.c_lflag,
        ]
    });
    let tty = io::stdout()
        .as_fd()
        .try_clone_to_owned()
        .ok()
        .map(File::from)
        .and_then(|file| file.metadata().ok())
        .map(|metadata| (metadata.dev(), metadata.ino()));
    let record = serde_json::json!({"time_unix_ms":timestamp,"pid":std::process::id(),
        "thread":format!("{:?}",std::thread::current().id()),"event":event,
        "output_identity":tty,"input_flags":flags,"error_os":error.and_then(io::Error::raw_os_error),
        "source":{"file":source.file(),"line":source.line()}});
    let mut encoded = serde_json::to_vec(&record)?;
    encoded.push(b'\n');
    const MAX_BYTES: u64 = 64 * 1024;
    if file.metadata()?.len().saturating_add(encoded.len() as u64) > MAX_BYTES {
        file.set_len(0)?;
    }
    file.write_all(&encoded)?;
    // Closing the descriptor releases the advisory lock even when an earlier step fails.
    Ok(())
}

// ^ docs/ui-state-ownership.md: crossterm's raw-mode flag does not own changes made by external editors.
struct TerminalAttributes {
    input: File,
    saved: libc::termios,
}

impl TerminalAttributes {
    fn capture() -> io::Result<Self> {
        // Match crossterm's documented stdin-or-controlling-terminal selection.
        let input = if io::stdin().is_terminal() {
            File::from(io::stdin().as_fd().try_clone_to_owned()?)
        } else {
            File::options().read(true).write(true).open("/dev/tty")?
        };
        let mut saved = std::mem::MaybeUninit::uninit();
        // SAFETY: the owned descriptor is live and the output buffer has native termios size.
        if unsafe { libc::tcgetattr(input.as_raw_fd(), saved.as_mut_ptr()) } < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful tcgetattr initializes the native attributes.
        Ok(Self {
            input,
            saved: unsafe { saved.assume_init() },
        })
    }

    fn restore(&self) -> io::Result<()> {
        // SAFETY: both the owned descriptor and captured native attributes remain valid.
        if unsafe { libc::tcsetattr(self.input.as_raw_fd(), libc::TCSANOW, &self.saved) } < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

// ^ docs/ui-state-ownership.md: only the thread owning the outer TUI may release its modes.
pub struct TerminalSession {
    active: Arc<AtomicBool>,
    worker_failure: Arc<Mutex<Option<String>>>,
    previous_hook: Arc<PanicHook>,
    output: Arc<File>,
    attributes: Arc<TerminalAttributes>,
    _owner_thread: PhantomData<Rc<()>>,
}

impl TerminalSession {
    fn new() -> Result<Self> {
        // ^ Cleanup targets the initialized stdout surface, even if stderr is redirected or stdout is locked.
        let output = Arc::new(File::from(io::stdout().as_fd().try_clone_to_owned()?));
        let attributes = Arc::new(TerminalAttributes::capture()?);
        record_mode_event("owner_acquired", None);
        let active = Arc::new(AtomicBool::new(true));
        let worker_failure = Arc::new(Mutex::new(None));
        let previous_hook: Arc<PanicHook> = Arc::from(std::panic::take_hook());
        let owner = std::thread::current().id();
        let hook_active = Arc::clone(&active);
        let hook_failure = Arc::clone(&worker_failure);
        let previous = Arc::clone(&previous_hook);
        let hook_output = Arc::clone(&output);
        let hook_attributes = Arc::clone(&attributes);
        std::panic::set_hook(Box::new(move |info| {
            if hook_active.load(Ordering::Acquire) {
                if std::thread::current().id() == owner {
                    hook_active.store(false, Ordering::Release);
                    let restored =
                        restore_modes(&mut hook_output.as_ref(), Some(hook_attributes.as_ref()));
                    record_mode_event_at(
                        if restored.is_ok() {
                            "owner_panic_released"
                        } else {
                            "owner_panic_release_failed"
                        },
                        restored.as_ref().err(),
                        info.location()
                            .unwrap_or_else(|| std::panic::Location::caller()),
                    );
                } else {
                    record_mode_event_at(
                        "worker_panic_preserved_modes",
                        None,
                        info.location()
                            .unwrap_or_else(|| std::panic::Location::caller()),
                    );
                    // Keep diagnostics off the live terminal byte stream. The owner displays them.
                    if let Ok(mut failure) = hook_failure.try_lock() {
                        if failure.is_none() {
                            let payload = info
                                .payload()
                                .downcast_ref::<&str>()
                                .copied()
                                .or_else(|| {
                                    info.payload().downcast_ref::<String>().map(String::as_str)
                                })
                                .unwrap_or("unknown panic");
                            let message: String = payload
                                .chars()
                                .filter(|ch| !ch.is_control())
                                .take(160)
                                .collect();
                            let location = info
                                .location()
                                .map(|location| format!("{}:{}", location.file(), location.line()))
                                .unwrap_or_else(|| "unknown location".into());
                            *failure = Some(
                                format!("Background worker failed at {location}: {message}")
                                    .chars()
                                    .filter(|ch| !ch.is_control())
                                    .take(320)
                                    .collect(),
                            );
                        }
                    }
                    return;
                }
            }
            previous(info);
        }));
        Ok(Self {
            active,
            worker_failure,
            previous_hook,
            output,
            attributes,
            _owner_thread: PhantomData,
        })
    }

    #[track_caller]
    pub fn record_failure(&self, phase: &'static str, error: &anyhow::Error) {
        record_mode_event(phase, error.downcast_ref::<io::Error>());
    }

    pub fn take_worker_failure(&self) -> Option<String> {
        self.worker_failure
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }

    pub fn restore(&mut self) -> Result<()> {
        if self.active.swap(false, Ordering::AcqRel) {
            let result = restore_modes(&mut self.output.as_ref(), Some(self.attributes.as_ref()));
            record_mode_event(
                if result.is_ok() {
                    "owner_released"
                } else {
                    "owner_release_failed"
                },
                result.as_ref().err(),
            );
            if result.is_err() {
                self.active.store(true, Ordering::Release);
            }
            result?;
        }
        Ok(())
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = self.restore();
        if !std::thread::panicking() {
            let previous = Arc::clone(&self.previous_hook);
            std::panic::set_hook(Box::new(move |info| previous(info)));
        }
    }
}

pub fn init() -> Result<(TerminalSession, Tui)> {
    // The guard exists before the first effect, including partial initialization errors.
    let session = TerminalSession::new()?;
    record_mode_event("initialization_requested", None);
    enable_raw_mode().map_err(|error| TerminalModeError::at("initial_raw_mode_failed", error))?;
    let mut stdout = io::stdout();
    enter_modes(&mut stdout)
        .map_err(|error| TerminalModeError::at("initial_output_modes_failed", error))?;
    let terminal = Terminal::new(CrosstermBackend::new(stdout))
        .map_err(|error| TerminalModeError::at("initial_backend_failed", error))?;
    record_mode_event("initialized", None);
    Ok((session, terminal))
}

fn enter_modes(writer: &mut impl Write) -> io::Result<()> {
    execute!(
        writer,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )
}

fn restore_modes(
    writer: &mut impl Write,
    attributes: Option<&TerminalAttributes>,
) -> io::Result<()> {
    // Attempt every cleanup even if raw-mode or an earlier output operation fails.
    let mut result = disable_raw_mode();
    if let Some(attributes) = attributes {
        let restored = attributes.restore();
        if result.is_ok() {
            result = restored;
        }
    }
    for next in [
        execute!(writer, EndSynchronizedUpdate),
        execute!(writer, DisableBracketedPaste),
        execute!(writer, DisableMouseCapture),
        execute!(writer, SetCursorStyle::DefaultUserShape),
        execute!(writer, LeaveAlternateScreen),
        execute!(writer, crossterm::cursor::Show),
    ] {
        if result.is_ok() {
            result = next;
        }
    }
    result
}

/// Draw with synchronized output to prevent terminal from rendering partial frames.
/// Terminal clears are reserved for resize; preview clears flush captured terminal glyphs.
pub fn draw_sync<F>(
    terminal: &mut Tui,
    clear_terminal: bool,
    clear_preview: bool,
    cursor: Option<Cursor>,
    mut render: F,
) -> Result<()>
where
    F: FnMut(&mut ratatui::Frame, bool),
{
    execute!(terminal.backend_mut(), BeginSynchronizedUpdate)?;
    let result = (|| -> Result<()> {
        if clear_terminal {
            terminal.clear()?;
            // terminal.clear() resets the back buffer but NOT the current (front) buffer,
            // so the next diff skips cells that match the stale front buffer even though
            // the screen was cleared. Drawing an empty frame first flushes the front buffer
            // to blank state, ensuring the real draw below writes every cell unconditionally.
            terminal.draw(|_| {})?;
        }
        if clear_preview {
            terminal.draw(|frame| render(frame, true))?;
        }
        terminal.draw(|frame| render(frame, false))?;
        execute!(terminal.backend_mut(), cursor_style(cursor))?;
        Ok(())
    })();
    let end = execute!(terminal.backend_mut(), EndSynchronizedUpdate);
    result?;
    end?;
    Ok(())
}

fn osc52_sequence(text: &[u8]) -> String {
    format!("\x1b]52;c;{}\x07", BASE64_STANDARD.encode(text))
}

pub fn copy_to_clipboard(text: &[u8]) -> Result<()> {
    let mut stdout = io::stdout().lock();
    write!(stdout, "{}", osc52_sequence(text))?;
    stdout.flush()?;
    Ok(())
}

#[derive(Debug)]
pub struct TerminalModeError {
    phase: &'static str,
    error: io::Error,
}

impl TerminalModeError {
    #[track_caller]
    fn at(phase: &'static str, error: io::Error) -> Self {
        record_mode_event(phase, Some(&error));
        Self { phase, error }
    }
}

impl std::fmt::Display for TerminalModeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "could not restore TUI terminal modes ({}): {}",
            self.phase, self.error
        )
    }
}

impl std::error::Error for TerminalModeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

/// Temporarily restore the shell terminal while running an interactive external command.
pub fn with_raw_mode_disabled<F, R>(terminal: &mut Tui, f: F) -> Result<R>
where
    F: FnOnce() -> Result<R>,
{
    let suspension = restore_modes(terminal.backend_mut(), None);
    record_mode_event(
        if suspension.is_ok() {
            "editor_suspended"
        } else {
            "editor_suspension_failed"
        },
        suspension.as_ref().err(),
    );
    let (result, attributes) = match suspension.and_then(|_| TerminalAttributes::capture()) {
        Ok(attributes) => (f(), Some(attributes)),
        Err(error) => (Err(error.into()), None),
    };
    // Restore the borrowed baseline before crossterm captures it again as its normal mode.
    if let Some(attributes) = attributes {
        attributes
            .restore()
            .map_err(|error| TerminalModeError::at("editor_input_restore_failed", error))?;
    }
    // A failed editor or suspension still returns control to the same terminal owner.
    enable_raw_mode().map_err(|error| TerminalModeError::at("editor_raw_resume_failed", error))?;
    enter_modes(terminal.backend_mut())
        .map_err(|error| TerminalModeError::at("editor_output_resume_failed", error))?;
    terminal
        .clear()
        .map_err(|error| TerminalModeError::at("editor_redraw_failed", error))?;
    record_mode_event(
        if result.is_ok() {
            "editor_returned"
        } else {
            "editor_returned_with_error"
        },
        None,
    );
    result
}

fn cursor_style(cursor: Option<Cursor>) -> SetCursorStyle {
    let Some(cursor) = cursor.filter(|cursor| cursor.visible) else {
        return SetCursorStyle::DefaultUserShape;
    };
    match (cursor.shape, cursor.blinking) {
        (0 | 3, true) => SetCursorStyle::BlinkingBlock,
        (0 | 3, false) => SetCursorStyle::SteadyBlock,
        (1, true) => SetCursorStyle::BlinkingUnderScore,
        (1, false) => SetCursorStyle::SteadyUnderScore,
        (2, true) => SetCursorStyle::BlinkingBar,
        (2, false) => SetCursorStyle::SteadyBar,
        _ => SetCursorStyle::DefaultUserShape,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_diagnostics_are_bounded_metadata_and_reject_unsafe_files() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let directory = std::env::current_dir()
            .unwrap()
            .join(".work")
            .join(format!("mode-diag-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("events.jsonl");
        append_mode_event(&path, "test", None).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let text = std::fs::read_to_string(&path).unwrap();
        let record: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(record["event"], "test");
        assert!(record.get("screen").is_none() && record.get("payload").is_none());
        std::fs::write(&path, vec![b'x'; 64 * 1024]).unwrap();
        append_mode_event(&path, "rotated", None).unwrap();
        assert!(std::fs::metadata(&path).unwrap().len() < 64 * 1024);
        assert!(std::fs::read_to_string(&path).unwrap().contains("rotated"));
        std::fs::remove_file(&path).unwrap();
        let target = directory.join("preserved");
        std::fs::write(&target, "preserve").unwrap();
        symlink(&target, &path).unwrap();
        assert!(append_mode_event(&path, "denied", None).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "preserve");
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, "preserve").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(append_mode_event(&path, "denied", None).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "preserve");
        std::fs::remove_dir_all(directory).unwrap();
    }

    // Real-PTY helper used by scripts/test-ui-state.py, not an always-green unit test.
    #[test]
    #[ignore = "requires a private real PTY supplied by scripts/test-ui-state.py"]
    fn terminal_owner_probe() {
        let (mut session, mut terminal) = init().unwrap();
        print!("PROBE_READY");
        io::stdout().flush().unwrap();
        loop {
            match crossterm::event::read().unwrap() {
                crossterm::event::Event::Key(key) => match key.code {
                    crossterm::event::KeyCode::Char('p') => {
                        assert!(std::thread::spawn(|| panic!("controlled worker failure"))
                            .join()
                            .is_err());
                        assert!(crossterm::terminal::is_raw_mode_enabled().unwrap());
                        let failure = session.take_worker_failure().unwrap();
                        assert!(failure.contains("controlled worker failure"));
                        let payload =
                            format!("controlled worker failure\n\x1b[?1006l{}", "x".repeat(1000));
                        assert!(std::thread::spawn(move || panic!("{payload}"))
                            .join()
                            .is_err());
                        let failure = session.take_worker_failure().unwrap();
                        assert!(failure.chars().count() <= 320);
                        assert!(!failure.chars().any(char::is_control));
                        print!("PROBE_WORKER_REPORTED");
                    }
                    crossterm::event::KeyCode::Char('e') => {
                        let result: Result<()> = with_raw_mode_disabled(&mut terminal, || {
                            assert!(!crossterm::terminal::is_raw_mode_enabled().unwrap());
                            let status = std::process::Command::new("/bin/sh")
                                .args(["-c", "/bin/stty raw -echo && exit 7"])
                                .status()
                                .unwrap();
                            assert_eq!(status.code(), Some(7));
                            anyhow::bail!("controlled editor failure after raw attribute change");
                        });
                        assert!(result.is_err());
                        assert!(crossterm::terminal::is_raw_mode_enabled().unwrap());
                        print!("PROBE_EDITOR_RETURNED");
                    }
                    crossterm::event::KeyCode::Char('x') => panic!("controlled owner failure"),
                    crossterm::event::KeyCode::Char('q') => {
                        session.restore().unwrap();
                        return;
                    }
                    _ => {}
                },
                crossterm::event::Event::Mouse(mouse) => print!("PROBE_MOUSE:{:?}", mouse.kind),
                _ => {}
            }
            io::stdout().flush().unwrap();
        }
    }

    #[test]
    fn clipboard_output_is_one_standard_osc52_sequence() {
        assert_eq!(osc52_sequence(b"copied"), "\x1b]52;c;Y29waWVk\x07");
    }

    #[test]
    fn maps_ghostty_cursor_shapes_and_blinking() {
        let cursor = |shape, blinking| Cursor {
            x: 0,
            y: 0,
            visible: true,
            blinking,
            shape,
        };
        assert_eq!(
            cursor_style(Some(cursor(0, false))).to_string(),
            "\u{1b}[2 q"
        );
        assert_eq!(
            cursor_style(Some(cursor(1, true))).to_string(),
            "\u{1b}[3 q"
        );
        assert_eq!(
            cursor_style(Some(cursor(2, false))).to_string(),
            "\u{1b}[6 q"
        );
        assert_eq!(
            cursor_style(Some(cursor(3, true))).to_string(),
            "\u{1b}[1 q"
        );
    }
}
