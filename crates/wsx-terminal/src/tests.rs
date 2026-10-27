use super::*;

// ^ docs/ui-state-ownership.md: observe final VT cells, not contiguous output bytes.
#[test]
#[ignore = "replays private PTY output supplied by scripts/test-ui-state.py"]
fn rendered_output_probe() {
    let path = std::path::PathBuf::from(std::env::var_os("WSX_RENDER_REPLAY").unwrap());
    let expected = std::env::var("WSX_RENDER_EXPECTED").unwrap();
    let rows = std::env::var("WSX_RENDER_ROWS")
        .map(|value| value.parse::<u16>().unwrap())
        .unwrap_or(24);
    let cols = std::env::var("WSX_RENDER_COLS")
        .map(|value| value.parse::<u16>().unwrap())
        .unwrap_or(100);
    // One guard row prevents bottom-right painting from scrolling the captured app.
    let runtime = TerminalRuntime::spawn(
        PaneId(1),
        TerminalId(1),
        path.parent().unwrap(),
        &["/bin/cat".into(), path.to_string_lossy().into_owned()],
        &[],
        None,
        rows.checked_add(1).unwrap(),
        cols,
        Arc::new(|| {}),
    )
    .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let frame = runtime.frame().unwrap();
        let text = frame
            .cells
            .chunks(usize::from(frame.cols))
            .map(|row| {
                row.iter()
                    .map(|cell| cell.symbol.as_str())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        if text.contains(&expected) {
            std::fs::write(path.with_extension("screen.txt"), text).unwrap();
            std::fs::write(path.with_extension("frame.txt"), format!("{frame:#?}")).unwrap();
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "rendered marker missing: {text:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    drop(runtime);
}

#[test]
fn natural_exit_waits_for_final_pty_effects_to_drain() {
    // Join the production waiter to establish reaping without relying on
    // platform-specific PTY close behavior or scheduling delays.
    let runtime = TerminalRuntime::new_for_test(2, 12).unwrap();
    let child = std::process::Command::new("/bin/sh")
        .args(["-c", "exit 0"])
        .spawn()
        .unwrap();
    spawn_waiter(
        Box::new(child),
        Arc::clone(&runtime.shared),
        Arc::new(std::sync::atomic::AtomicI32::new(0)),
    )
    .unwrap()
    .join()
    .unwrap();
    assert!(!runtime.exited(), "child exit bypassed final PTY drain");
    {
        let mut emulator = lock(&runtime.shared.emulator);
        emulator.terminal.write(b"\x1b]52;c;ZXhpdA==\x07");
    }
    // Reader EOF publishes exit after consuming its last output batch.
    mark_exited(&runtime.shared);
    assert!(runtime.exited());
    assert_eq!(
        runtime.presentation_sample(None, true).clipboard_writes,
        vec![b"exit".to_vec()]
    );
}

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
fn agent_observation_reads_bounded_live_bottom_and_osc_title() {
    let runtime = TerminalRuntime::new_for_test(4, 24).unwrap();
    {
        let mut emulator = lock(&runtime.shared.emulator);
        emulator.terminal.write(
            b"old-one\r\nold-two\r\nold-three\r\ncurrent-one\r\ncurrent-two\x1b]2;\xe2\xa0\x8b task\x07",
        );
    }

    let observation = runtime.agent_observation().unwrap();

    assert_eq!(observation.title, "⠋ task");
    assert!(!observation.text.contains("old-one"));
    assert!(observation.text.contains("current-one"));
    assert!(observation.text.contains("current-two"));
    assert!(observation.text.len() <= AGENT_OBSERVATION_TEXT_BYTES);
    assert!(observation.title.len() <= AGENT_OBSERVATION_TITLE_BYTES);
}

#[test]
fn agent_observation_reports_runtime_errors_without_partial_evidence() {
    let runtime = TerminalRuntime::new_for_test(2, 8).unwrap();
    *lock(&runtime.shared.error) = Some("reader failed".into());

    assert!(matches!(
        runtime.agent_observation(),
        Err(TerminalError::Runtime(message)) if message == "reader failed"
    ));
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
fn presentation_sample_defers_frames_without_deferring_effects() {
    let runtime = TerminalRuntime::new_for_test(2, 8).unwrap();
    let initial = match runtime.frame_update(None).unwrap() {
        TerminalUpdate::Full(frame) => frame,
        TerminalUpdate::Patch { .. } => panic!("expected initial full frame"),
    };
    let baseline = initial.revision;
    {
        let mut emulator = lock(&runtime.shared.emulator);
        emulator.terminal.write(b"A\x1b]52;c;Y29waWVk\x07");
        emulator.sync_input();
    }
    runtime.shared.revision.fetch_add(1, Ordering::AcqRel);

    let deferred = runtime.presentation_sample(Some(baseline), false);
    assert!(deferred.update.as_ref().unwrap().is_none());
    assert_eq!(deferred.clipboard_writes, vec![b"copied".to_vec()]);

    {
        let mut emulator = lock(&runtime.shared.emulator);
        emulator.terminal.write(b"B");
    }
    runtime.shared.revision.fetch_add(1, Ordering::AcqRel);
    let emitted = runtime.presentation_sample(Some(baseline), true);
    let mut latest = Some(initial);
    emitted
        .update
        .as_ref()
        .unwrap()
        .clone()
        .unwrap()
        .apply_to(&mut latest)
        .unwrap();
    assert_eq!(
        latest.unwrap().cells[0..2]
            .iter()
            .map(|cell| cell.symbol.as_str())
            .collect::<String>(),
        "AB"
    );
    assert!(emitted.clipboard_writes.is_empty());

    {
        let mut emulator = lock(&runtime.shared.emulator);
        emulator.terminal.write(b"\x1b]52;c;ZXJyb3I=\x07");
    }
    *lock(&runtime.shared.error) = Some("failed".into());
    let failed = runtime.presentation_sample(Some(emitted.revision), true);
    assert_eq!(failed.clipboard_writes, vec![b"error".to_vec()]);
    assert!(failed.update.is_err());
}

#[test]
fn presentation_sample_keeps_mode_revision_frame_and_effects_under_one_lock() {
    let runtime = TerminalRuntime::new_for_test(2, 8).unwrap();
    {
        let mut emulator = lock(&runtime.shared.emulator);
        emulator
            .terminal
            .write(b"\x1b[?2026hA\x1b]52;c;Zmlyc3Q=\x07\x1b]52;c;c2Vjb25k\x07");
        emulator.sync_input();
    }
    runtime.shared.revision.fetch_add(1, Ordering::AcqRel);

    let initial = runtime.presentation_sample(None, true);
    let baseline = initial
        .update
        .as_ref()
        .unwrap()
        .as_ref()
        .unwrap()
        .revision();
    assert!(initial.synchronized_output);
    assert_eq!(
        initial.clipboard_writes,
        vec![b"first".to_vec(), b"second".to_vec()]
    );

    {
        let mut emulator = lock(&runtime.shared.emulator);
        emulator.terminal.write(b"B");
    }
    runtime.shared.revision.fetch_add(1, Ordering::AcqRel);
    let suppressed = runtime.presentation_sample(Some(baseline), true);
    assert!(suppressed.synchronized_output);
    assert!(suppressed.update.as_ref().unwrap().is_none());

    {
        let mut emulator = lock(&runtime.shared.emulator);
        emulator.terminal.write(b"\x1b[?2026l");
    }
    runtime.shared.revision.fetch_add(1, Ordering::AcqRel);
    let released = runtime.presentation_sample(Some(baseline), true);
    assert!(!released.synchronized_output);
    assert!(released.update.as_ref().unwrap().is_some());
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

#[test]
fn spawned_process_receives_bounded_runtime_environment() {
    let command = vec![
        "/bin/sh".into(),
        "-c".into(),
        "printf '%s:%s' \"$WSX_TEST_MARKER\" \"$CLAUDE_CODE_CHILD_SESSION\"; sleep 1".into(),
    ];
    let runtime = TerminalRuntime::spawn(
        PaneId(1),
        TerminalId(2),
        &std::env::current_dir().unwrap(),
        &command,
        &[
            ("WSX_TEST_MARKER".into(), "pane-42".into()),
            ("CLAUDE_CODE_CHILD_SESSION".into(), "nested-child".into()),
        ],
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
        if text.contains("pane-42:nested-child") {
            observed = true;
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    runtime.terminate();
    assert!(observed);
}

// ^ A query reply must not block the reader: a child that reads no input and floods its
// output would otherwise wait for the reader while the reader waits for its input.
#[test]
fn query_reply_to_a_child_that_reads_no_input_does_not_stall_its_output() {
    let flag = std::env::temp_dir().join(format!("wsx-reply-stall-{}", std::process::id()));
    let _ = std::fs::remove_file(&flag);
    let script = format!(
        "stty raw -echo; printf ready; while [ ! -e '{}' ]; do sleep 0.05; done; \
         printf '\\033[c'; head -c 3000000 /dev/zero | tr '\\0' x",
        flag.display()
    );
    let runtime = TerminalRuntime::spawn(
        PaneId(1),
        TerminalId(2),
        &std::env::current_dir().unwrap(),
        &["/bin/sh".into(), "-c".into(), script],
        &[],
        None,
        4,
        40,
        Arc::new(|| {}),
    )
    .unwrap();
    let wait_until = |what: &str, done: &dyn Fn() -> bool| {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !done() {
            assert!(Instant::now() < deadline, "timed out: {what}");
            thread::sleep(Duration::from_millis(20));
        }
    };
    wait_until("child ready", &|| {
        runtime
            .frame()
            .unwrap()
            .cells
            .iter()
            .map(|cell| cell.symbol.as_str())
            .collect::<String>()
            .contains("ready")
    });
    // Fill the input queue so that any write to the PTY blocks.
    assert!(runtime
        .write_within(
            &vec![b'a'; 1 << 20],
            Instant::now() + Duration::from_millis(500)
        )
        .is_err());
    std::fs::write(&flag, b"").unwrap();
    wait_until("child output drained", &|| runtime.exited());
    let _ = std::fs::remove_file(&flag);
    runtime.terminate();
}

#[test]
fn interactive_shell_reports_a_distinct_foreground_job_group() {
    let runtime = TerminalRuntime::spawn(
        PaneId(1),
        TerminalId(2),
        &std::env::current_dir().unwrap(),
        &["/bin/sh".into()],
        &[],
        None,
        4,
        40,
        Arc::new(|| {}),
    )
    .unwrap();
    runtime.write(b"sleep 5\r").unwrap();
    let mut observed = false;
    for _ in 0..100 {
        if runtime.has_foreground_job() {
            observed = true;
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    runtime.terminate();
    assert!(observed);
}

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

fn wheel_up() -> MouseEvent {
    MouseEvent {
        action: MouseAction::Press,
        button: MouseButton::WheelUp,
        x: 0,
        y: 0,
        shift: false,
        control: false,
        alt: false,
        super_key: false,
        in_bounds: true,
    }
}

fn left_mouse(action: MouseAction, x: u16, y: u16, shift: bool) -> MouseEvent {
    MouseEvent {
        action,
        button: MouseButton::Left,
        x,
        y,
        in_bounds: true,
        shift,
        control: false,
        alt: false,
        super_key: false,
    }
}

fn wheel_down() -> MouseEvent {
    MouseEvent {
        button: MouseButton::WheelDown,
        ..wheel_up()
    }
}

struct RecordingWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl Write for RecordingWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        lock(&self.0).extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn recording_writer(runtime: &TerminalRuntime) -> std::sync::Arc<std::sync::Mutex<Vec<u8>>> {
    let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    *lock(&runtime.shared.writer) = Some(Box::new(RecordingWriter(captured.clone())));
    captured
}

fn frame_rows(frame: &TerminalFrame) -> Vec<String> {
    frame
        .cells
        .chunks(usize::from(frame.cols))
        .map(|row| row.iter().map(|cell| cell.symbol.as_str()).collect())
        .collect()
}

fn runtime_with_scrollback() -> TerminalRuntime {
    let runtime = TerminalRuntime::new_for_test(3, 4).unwrap();
    let mut emulator = lock(&runtime.shared.emulator);
    for line in 0..9 {
        let text = if line == 8 {
            format!("{line:02}")
        } else {
            format!("{line:02}\r\n")
        };
        emulator.terminal.write(text.as_bytes());
    }
    drop(emulator);
    runtime
}

#[test]
fn ghostty_autoscroll_tick_moves_viewport_and_selection_together_in_both_directions() {
    let runtime = runtime_with_scrollback();
    {
        let mut emulator = lock(&runtime.shared.emulator);
        emulator.terminal.selection_press(1, 2, 1).unwrap();
        emulator
            .terminal
            .selection_drag(0, 0, ghostty::SelectionBoundary::Above)
            .unwrap();
        assert_eq!(
            emulator.terminal.selection_autoscroll().unwrap(),
            ghostty::SelectionAutoscroll::Up
        );
        assert!(emulator.terminal.selection_autoscroll_tick(0, 0).unwrap());
    }
    assert_eq!(
        frame_rows(&runtime.frame().unwrap()),
        vec!["05  ".to_string(), "06  ".to_string(), "07  ".to_string()]
    );
    {
        let mut emulator = lock(&runtime.shared.emulator);
        let (_, copied) = emulator.terminal.selection_release(Some((0, 0))).unwrap();
        assert_eq!(copied, Some(b"05\n06\n07\n08".to_vec()));
        emulator.terminal.clear_selection().unwrap();
        emulator.terminal.selection_press(0, 0, 2).unwrap();
        emulator
            .terminal
            .selection_drag(1, 2, ghostty::SelectionBoundary::Below)
            .unwrap();
        assert_eq!(
            emulator.terminal.selection_autoscroll().unwrap(),
            ghostty::SelectionAutoscroll::Down
        );
        assert!(emulator.terminal.selection_autoscroll_tick(1, 2).unwrap());
    }
    assert_eq!(
        frame_rows(&runtime.frame().unwrap()),
        vec!["06  ".to_string(), "07  ".to_string(), "08  ".to_string()]
    );
    let mut emulator = lock(&runtime.shared.emulator);
    let (_, copied) = emulator.terminal.selection_release(Some((1, 2))).unwrap();
    assert_eq!(copied, Some(b"05\n06\n07\n08".to_vec()));
}

#[test]
fn outside_local_drag_starts_and_release_stops_runtime_autoscroll() {
    let runtime = runtime_with_scrollback();
    let before = frame_rows(&runtime.frame().unwrap());
    let revision = runtime.revision();
    runtime
        .mouse(&left_mouse(MouseAction::Press, 1, 2, false))
        .unwrap();
    runtime
        .mouse(&MouseEvent {
            in_bounds: false,
            ..left_mouse(MouseAction::Motion, 0, 0, false)
        })
        .unwrap();

    let deadline = Instant::now() + Duration::from_millis(500);
    while frame_rows(&runtime.frame().unwrap()) == before && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert_ne!(frame_rows(&runtime.frame().unwrap()), before);
    assert!(runtime.revision() > revision);

    runtime
        .mouse(&MouseEvent {
            in_bounds: false,
            ..left_mouse(MouseAction::Release, 0, 0, false)
        })
        .unwrap();
    assert!(!lock(&runtime.shared.emulator).local_selection_active);
    assert!(lock(&runtime.shared.emulator)
        .selection_autoscroll
        .is_none());
    assert!(!runtime.take_clipboard_writes().is_empty());
}

#[test]
fn runtime_exit_cancels_selection_autoscroll() {
    let runtime = runtime_with_scrollback();
    runtime
        .mouse(&left_mouse(MouseAction::Press, 1, 2, false))
        .unwrap();
    runtime
        .mouse(&MouseEvent {
            in_bounds: false,
            ..left_mouse(MouseAction::Motion, 0, 0, false)
        })
        .unwrap();
    assert!(lock(&runtime.shared.emulator)
        .selection_autoscroll
        .is_some());

    runtime.terminate();

    assert!(runtime.exited());
    let emulator = lock(&runtime.shared.emulator);
    assert!(!emulator.local_selection_active);
    assert!(emulator.selection_autoscroll.is_none());
}

#[test]
fn alternate_screen_outside_drag_does_not_shift_content_or_keep_a_timer_alive() {
    let runtime = TerminalRuntime::new_for_test(3, 8).unwrap();
    {
        let mut emulator = lock(&runtime.shared.emulator);
        emulator.terminal.write(b"primary\x1b[?1049h\x1b[HALT");
    }
    let before = frame_rows(&runtime.frame().unwrap());
    runtime
        .mouse(&left_mouse(MouseAction::Press, 0, 0, false))
        .unwrap();
    runtime
        .mouse(&MouseEvent {
            in_bounds: false,
            ..left_mouse(MouseAction::Motion, 0, 0, false)
        })
        .unwrap();

    let deadline = Instant::now() + Duration::from_millis(500);
    while lock(&runtime.shared.emulator)
        .selection_autoscroll
        .is_some()
        && Instant::now() < deadline
    {
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(frame_rows(&runtime.frame().unwrap()), before);
    assert!(lock(&runtime.shared.emulator)
        .selection_autoscroll
        .is_none());

    runtime
        .mouse(&MouseEvent {
            in_bounds: false,
            ..left_mouse(MouseAction::Release, 0, 0, false)
        })
        .unwrap();
    assert!(!lock(&runtime.shared.emulator).local_selection_active);
}

#[test]
fn primary_wheel_updates_the_frame_and_revision_immediately() {
    let runtime = runtime_with_scrollback();
    let captured = recording_writer(&runtime);
    let revision = runtime.revision();

    runtime.mouse(&wheel_up()).unwrap();

    assert_eq!(
        (frame_rows(&runtime.frame().unwrap()), runtime.revision()),
        (
            vec!["03  ".into(), "04  ".into(), "05  ".into()],
            revision + 1
        )
    );
    assert!(lock(&captured).is_empty());
}

#[test]
fn primary_wheel_at_the_bottom_of_scrollback_is_a_revision_no_op() {
    let runtime = runtime_with_scrollback();
    let captured = recording_writer(&runtime);
    let before = runtime.frame().unwrap();
    let revision = runtime.revision();

    runtime.mouse(&wheel_down()).unwrap();

    assert_eq!(
        (frame_rows(&runtime.frame().unwrap()), runtime.revision()),
        (frame_rows(&before), revision)
    );
    assert!(lock(&captured).is_empty());
}

#[test]
fn primary_wheel_at_the_top_of_scrollback_is_a_revision_no_op() {
    let runtime = runtime_with_scrollback();
    let captured = recording_writer(&runtime);
    runtime.mouse(&wheel_up()).unwrap();
    runtime.mouse(&wheel_up()).unwrap();
    let before = runtime.frame().unwrap();
    let revision = runtime.revision();

    runtime.mouse(&wheel_up()).unwrap();

    assert_eq!(
        (frame_rows(&runtime.frame().unwrap()), runtime.revision()),
        (frame_rows(&before), revision)
    );
    assert!(lock(&captured).is_empty());
}

#[test]
fn primary_wheel_down_after_wheel_up_moves_exactly_three_rows() {
    let runtime = runtime_with_scrollback();
    let captured = recording_writer(&runtime);
    runtime.mouse(&wheel_up()).unwrap();
    let revision = runtime.revision();

    runtime.mouse(&wheel_down()).unwrap();

    assert_eq!(
        (frame_rows(&runtime.frame().unwrap()), runtime.revision()),
        (
            vec!["06  ".into(), "07  ".into(), "08  ".into()],
            revision + 1
        )
    );
    assert!(lock(&captured).is_empty());
}

#[test]
fn reported_primary_wheel_does_not_change_the_local_viewport() {
    let runtime = runtime_with_scrollback();
    let captured = recording_writer(&runtime);
    {
        let mut emulator = lock(&runtime.shared.emulator);
        emulator.terminal.write(b"\x1b[?1000h\x1b[?1006h");
    }
    let before = runtime.frame().unwrap();
    let revision = runtime.revision();

    runtime.mouse(&wheel_up()).unwrap();

    assert_eq!(
        (frame_rows(&runtime.frame().unwrap()), runtime.revision()),
        (frame_rows(&before), revision)
    );
    assert_eq!(&*lock(&captured), b"\x1b[<64;1;1M");
}

#[test]
fn alternate_scroll_wheel_does_not_change_the_local_viewport() {
    let runtime = TerminalRuntime::new_for_test(3, 4).unwrap();
    let captured = recording_writer(&runtime);
    {
        let mut emulator = lock(&runtime.shared.emulator);
        emulator.terminal.write(b"ALT\x1b[?1049h\x1b[?1007h");
    }
    let before = runtime.frame().unwrap();
    let revision = runtime.revision();

    runtime.mouse(&wheel_up()).unwrap();

    assert_eq!(
        (frame_rows(&runtime.frame().unwrap()), runtime.revision()),
        (frame_rows(&before), revision)
    );
    assert_eq!(&*lock(&captured), b"\x1b[A\x1b[A\x1b[A");
}

#[test]
fn local_drag_selection_is_projected_and_copied_on_release() {
    let runtime = TerminalRuntime::new_for_test(2, 8).unwrap();
    {
        let mut emulator = lock(&runtime.shared.emulator);
        emulator.terminal.write(b"hello");
    }
    let captured = recording_writer(&runtime);

    runtime
        .mouse(&left_mouse(MouseAction::Press, 0, 0, false))
        .unwrap();
    runtime
        .mouse(&left_mouse(MouseAction::Motion, 4, 0, false))
        .unwrap();

    let frame = match runtime.frame_update(None).unwrap() {
        TerminalUpdate::Full(frame) => frame,
        TerminalUpdate::Patch { .. } => panic!("expected full frame"),
    };
    assert_eq!(
        frame.selection,
        vec![TerminalSelectionRange {
            row: 0,
            start_col: 0,
            end_col: 4,
        }]
    );
    assert!(runtime.frame().unwrap().selection.is_empty());

    runtime
        .mouse(&left_mouse(MouseAction::Release, 4, 0, false))
        .unwrap();
    assert_eq!(runtime.take_clipboard_writes(), vec![b"hello".to_vec()]);
    assert!(lock(&captured).is_empty());

    runtime.clear_selection().unwrap();
    let frame = match runtime.frame_update(None).unwrap() {
        TerminalUpdate::Full(frame) => frame,
        TerminalUpdate::Patch { .. } => panic!("expected full frame"),
    };
    assert!(frame.selection.is_empty());
}

#[test]
fn release_at_a_new_cell_publishes_the_final_selection_without_a_motion_event() {
    let runtime = TerminalRuntime::new_for_test(2, 8).unwrap();
    {
        let mut emulator = lock(&runtime.shared.emulator);
        emulator.terminal.write(b"hello");
    }
    let revision = runtime.revision();
    runtime
        .mouse(&left_mouse(MouseAction::Press, 0, 0, false))
        .unwrap();
    runtime
        .mouse(&left_mouse(MouseAction::Release, 4, 0, false))
        .unwrap();

    assert_eq!(runtime.take_clipboard_writes(), vec![b"hello".to_vec()]);
    assert!(runtime.revision() > revision);
    assert_eq!(
        match runtime.frame_update(None).unwrap() {
            TerminalUpdate::Full(frame) => frame.selection,
            TerminalUpdate::Patch { .. } => panic!("expected full frame"),
        },
        vec![TerminalSelectionRange {
            row: 0,
            start_col: 0,
            end_col: 4,
        }]
    );
}

#[test]
fn viewport_movement_ends_the_gesture_and_clears_selection() {
    let runtime = runtime_with_scrollback();
    runtime
        .mouse(&left_mouse(MouseAction::Press, 0, 2, false))
        .unwrap();
    runtime
        .mouse(&left_mouse(MouseAction::Motion, 1, 2, false))
        .unwrap();
    assert!(!match runtime.frame_update(None).unwrap() {
        TerminalUpdate::Full(frame) => frame.selection,
        TerminalUpdate::Patch { .. } => panic!("expected full frame"),
    }
    .is_empty());

    runtime.mouse(&wheel_up()).unwrap();

    assert!(match runtime.frame_update(None).unwrap() {
        TerminalUpdate::Full(frame) => frame.selection,
        TerminalUpdate::Patch { .. } => panic!("expected full frame"),
    }
    .is_empty());
    assert!(!lock(&runtime.shared.emulator).local_selection_active);
    assert!(runtime.take_clipboard_writes().is_empty());
}

#[test]
fn alternate_screen_transition_clears_selection() {
    let runtime = TerminalRuntime::new_for_test(2, 8).unwrap();
    {
        let mut emulator = lock(&runtime.shared.emulator);
        emulator.terminal.write(b"hello");
    }
    runtime
        .mouse(&left_mouse(MouseAction::Press, 0, 0, false))
        .unwrap();
    runtime
        .mouse(&left_mouse(MouseAction::Motion, 4, 0, false))
        .unwrap();
    {
        let mut emulator = lock(&runtime.shared.emulator);
        emulator.terminal.write(b"\x1b[?1049h");
    }

    assert!(match runtime.frame_update(None).unwrap() {
        TerminalUpdate::Full(frame) => frame.selection,
        TerminalUpdate::Patch { .. } => panic!("expected full frame"),
    }
    .is_empty());
}

#[test]
fn resize_clears_selection_before_the_new_geometry_is_presented() {
    let runtime = TerminalRuntime::spawn(
        PaneId(1),
        TerminalId(2),
        &std::env::current_dir().unwrap(),
        &["/bin/sh".into(), "-c".into(), "sleep 2".into()],
        &[],
        None,
        2,
        8,
        Arc::new(|| {}),
    )
    .unwrap();
    {
        let mut emulator = lock(&runtime.shared.emulator);
        emulator.terminal.write(b"hello");
    }
    runtime
        .mouse(&left_mouse(MouseAction::Press, 0, 0, false))
        .unwrap();
    runtime
        .mouse(&left_mouse(MouseAction::Motion, 4, 0, false))
        .unwrap();

    runtime.resize(3, 10).unwrap();

    assert!(match runtime.frame_update(None).unwrap() {
        TerminalUpdate::Full(frame) => frame.selection,
        TerminalUpdate::Patch { .. } => panic!("expected full frame"),
    }
    .is_empty());
    assert!(!lock(&runtime.shared.emulator).local_selection_active);
    runtime.terminate();
}

#[test]
fn selection_copy_follows_prior_child_clipboard_effects_in_one_fifo() {
    let runtime = TerminalRuntime::new_for_test(2, 8).unwrap();
    {
        let mut emulator = lock(&runtime.shared.emulator);
        emulator.terminal.write(b"hello\x1b]52;c;Zmlyc3Q=\x07");
    }
    runtime
        .mouse(&left_mouse(MouseAction::Press, 0, 0, false))
        .unwrap();
    runtime
        .mouse(&left_mouse(MouseAction::Motion, 4, 0, false))
        .unwrap();
    runtime
        .mouse(&left_mouse(MouseAction::Release, 4, 0, false))
        .unwrap();

    assert_eq!(
        runtime.take_clipboard_writes(),
        vec![b"first".to_vec(), b"hello".to_vec()]
    );
}

#[test]
fn selection_only_patches_replace_the_semantic_selection_snapshot() {
    let runtime = TerminalRuntime::new_for_test(2, 8).unwrap();
    {
        let mut emulator = lock(&runtime.shared.emulator);
        emulator.terminal.write(b"hello");
    }
    let mut baseline = Some(match runtime.frame_update(None).unwrap() {
        TerminalUpdate::Full(frame) => frame,
        TerminalUpdate::Patch { .. } => panic!("expected full frame"),
    });
    let base_revision = baseline.as_ref().unwrap().revision;

    runtime
        .mouse(&left_mouse(MouseAction::Press, 0, 0, false))
        .unwrap();
    runtime
        .mouse(&left_mouse(MouseAction::Motion, 4, 0, false))
        .unwrap();
    let selected_revision = runtime.revision();
    runtime
        .frame_update(Some(base_revision))
        .unwrap()
        .apply_to(&mut baseline)
        .unwrap();
    assert_eq!(baseline.as_ref().unwrap().selection.len(), 1);

    runtime.clear_selection().unwrap();
    runtime
        .frame_update(Some(selected_revision))
        .unwrap()
        .apply_to(&mut baseline)
        .unwrap();
    assert!(baseline.unwrap().selection.is_empty());
}

#[test]
fn child_mouse_reporting_wins_unless_shift_latches_local_selection() {
    let runtime = TerminalRuntime::new_for_test(2, 8).unwrap();
    let captured = recording_writer(&runtime);
    {
        let mut emulator = lock(&runtime.shared.emulator);
        emulator.terminal.write(b"hello\x1b[?1000h\x1b[?1006h");
    }

    runtime
        .mouse(&left_mouse(MouseAction::Press, 0, 0, false))
        .unwrap();
    runtime
        .mouse(&left_mouse(MouseAction::Release, 0, 0, false))
        .unwrap();
    assert_eq!(&*lock(&captured), b"\x1b[<0;1;1M\x1b[<0;1;1m");
    lock(&captured).clear();

    runtime
        .mouse(&left_mouse(MouseAction::Press, 0, 0, true))
        .unwrap();
    runtime
        .mouse(&left_mouse(MouseAction::Motion, 4, 0, false))
        .unwrap();
    runtime
        .mouse(&left_mouse(MouseAction::Release, 4, 0, false))
        .unwrap();

    assert!(lock(&captured).is_empty());
    assert_eq!(runtime.take_clipboard_writes(), vec![b"hello".to_vec()]);
}

#[test]
fn local_selection_stays_latched_if_mouse_reporting_changes_mid_drag() {
    let runtime = TerminalRuntime::new_for_test(2, 8).unwrap();
    let captured = recording_writer(&runtime);
    {
        let mut emulator = lock(&runtime.shared.emulator);
        emulator.terminal.write(b"hello");
    }

    runtime
        .mouse(&left_mouse(MouseAction::Press, 0, 0, false))
        .unwrap();
    {
        let mut emulator = lock(&runtime.shared.emulator);
        emulator.terminal.write(b"\x1b[?1000h\x1b[?1006h");
    }
    runtime
        .mouse(&left_mouse(MouseAction::Motion, 4, 0, false))
        .unwrap();
    runtime
        .mouse(&left_mouse(MouseAction::Release, 4, 0, false))
        .unwrap();

    assert!(lock(&captured).is_empty());
    assert_eq!(runtime.take_clipboard_writes(), vec![b"hello".to_vec()]);
}

#[test]
fn child_mouse_route_stays_latched_through_mode_change_and_outside_release() {
    let runtime = TerminalRuntime::new_for_test(2, 8).unwrap();
    let captured = recording_writer(&runtime);
    {
        let mut emulator = lock(&runtime.shared.emulator);
        emulator.terminal.write(b"\x1b[?1002h\x1b[?1006h");
    }
    runtime
        .mouse(&left_mouse(MouseAction::Press, 0, 0, false))
        .unwrap();
    {
        let mut emulator = lock(&runtime.shared.emulator);
        emulator.terminal.write(b"\x1b[?1002l\x1b[?1006l");
    }
    runtime
        .mouse(&left_mouse(MouseAction::Motion, 3, 0, true))
        .unwrap();
    runtime
        .mouse(&MouseEvent {
            in_bounds: false,
            ..left_mouse(MouseAction::Release, 0, 0, true)
        })
        .unwrap();

    assert_eq!(&*lock(&captured), b"\x1b[<0;1;1M\x1b[<36;4;1M\x1b[<4;4;1m");
    assert!(lock(&runtime.shared.emulator).child_mouse_gesture.is_none());
    assert!(runtime.take_clipboard_writes().is_empty());
}

#[test]
fn reverse_drag_unwraps_soft_wrapped_rows_for_copy() {
    let runtime = TerminalRuntime::new_for_test(2, 4).unwrap();
    {
        let mut emulator = lock(&runtime.shared.emulator);
        emulator.terminal.write(b"abcdef");
    }
    runtime
        .mouse(&left_mouse(MouseAction::Press, 1, 1, false))
        .unwrap();
    runtime
        .mouse(&left_mouse(MouseAction::Motion, 1, 0, false))
        .unwrap();
    let frame = match runtime.frame_update(None).unwrap() {
        TerminalUpdate::Full(frame) => frame,
        TerminalUpdate::Patch { .. } => panic!("expected full frame"),
    };
    assert_eq!(
        frame.selection,
        vec![
            TerminalSelectionRange {
                row: 0,
                start_col: 1,
                end_col: 3,
            },
            TerminalSelectionRange {
                row: 1,
                start_col: 0,
                end_col: 1,
            },
        ]
    );
    runtime
        .mouse(&left_mouse(MouseAction::Release, 1, 0, false))
        .unwrap();
    assert_eq!(runtime.take_clipboard_writes(), vec![b"bcdef".to_vec()]);
}

#[test]
fn wide_cell_selection_copies_one_grapheme() {
    let runtime = TerminalRuntime::new_for_test(1, 4).unwrap();
    {
        let mut emulator = lock(&runtime.shared.emulator);
        emulator.terminal.write("界a".as_bytes());
    }
    runtime
        .mouse(&left_mouse(MouseAction::Press, 0, 0, false))
        .unwrap();
    runtime
        .mouse(&left_mouse(MouseAction::Motion, 1, 0, false))
        .unwrap();
    runtime
        .mouse(&left_mouse(MouseAction::Release, 1, 0, false))
        .unwrap();
    assert_eq!(
        runtime.take_clipboard_writes(),
        vec!["界".as_bytes().to_vec()]
    );
}

#[test]
fn single_click_clears_an_old_selection_without_copying() {
    let runtime = TerminalRuntime::new_for_test(2, 8).unwrap();
    {
        let mut emulator = lock(&runtime.shared.emulator);
        emulator.terminal.write(b"hello x");
    }
    runtime
        .mouse(&left_mouse(MouseAction::Press, 0, 0, false))
        .unwrap();
    runtime
        .mouse(&left_mouse(MouseAction::Motion, 4, 0, false))
        .unwrap();
    runtime
        .mouse(&left_mouse(MouseAction::Release, 4, 0, false))
        .unwrap();
    let _ = runtime.take_clipboard_writes();

    runtime
        .mouse(&left_mouse(MouseAction::Press, 6, 0, false))
        .unwrap();
    runtime
        .mouse(&left_mouse(MouseAction::Release, 6, 0, false))
        .unwrap();

    assert!(runtime.take_clipboard_writes().is_empty());
    let frame = match runtime.frame_update(None).unwrap() {
        TerminalUpdate::Full(frame) => frame,
        TerminalUpdate::Patch { .. } => panic!("expected full frame"),
    };
    assert!(frame.selection.is_empty());
}

#[test]
fn dragging_back_to_the_cell_anchor_collapses_without_copying() {
    let runtime = TerminalRuntime::new_for_test(2, 8).unwrap();
    {
        let mut emulator = lock(&runtime.shared.emulator);
        emulator.terminal.write(b"hello");
    }
    runtime
        .mouse(&left_mouse(MouseAction::Press, 0, 0, false))
        .unwrap();
    runtime
        .mouse(&left_mouse(MouseAction::Motion, 4, 0, false))
        .unwrap();
    runtime
        .mouse(&left_mouse(MouseAction::Motion, 0, 0, false))
        .unwrap();
    runtime
        .mouse(&left_mouse(MouseAction::Release, 0, 0, false))
        .unwrap();

    assert!(runtime.take_clipboard_writes().is_empty());
    assert!(match runtime.frame_update(None).unwrap() {
        TerminalUpdate::Full(frame) => frame.selection,
        TerminalUpdate::Patch { .. } => panic!("expected full frame"),
    }
    .is_empty());
}

#[test]
fn repeated_clicks_use_ghostty_word_and_line_selection() {
    let runtime = TerminalRuntime::new_for_test(2, 16).unwrap();
    {
        let mut emulator = lock(&runtime.shared.emulator);
        emulator.terminal.write(b"hello world");
    }
    for expected in [None, Some("hello"), Some("hello world")] {
        runtime
            .mouse(&left_mouse(MouseAction::Press, 1, 0, false))
            .unwrap();
        runtime
            .mouse(&left_mouse(MouseAction::Release, 1, 0, false))
            .unwrap();
        assert_eq!(
            runtime
                .take_clipboard_writes()
                .into_iter()
                .next()
                .map(|bytes| String::from_utf8(bytes).unwrap()),
            expected.map(str::to_owned)
        );
    }
}

#[test]
fn pointer_boundary_rejects_fabricated_in_grid_cells_and_ignores_outside_motion() {
    let runtime = TerminalRuntime::new_for_test(2, 8).unwrap();
    let captured = recording_writer(&runtime);

    assert!(runtime
        .mouse(&left_mouse(MouseAction::Press, 8, 0, false))
        .is_err());
    let mut outside_motion = left_mouse(MouseAction::Motion, u16::MAX, u16::MAX, false);
    outside_motion.in_bounds = false;
    runtime.mouse(&outside_motion).unwrap();

    assert!(!lock(&runtime.shared.emulator).local_selection_active);
    assert!(lock(&captured).is_empty());
}

#[test]
fn outside_release_ends_local_selection_without_fabricating_a_cell() {
    let runtime = TerminalRuntime::new_for_test(2, 8).unwrap();
    {
        let mut emulator = lock(&runtime.shared.emulator);
        emulator.terminal.write(b"hello");
    }
    runtime
        .mouse(&left_mouse(MouseAction::Press, 0, 0, false))
        .unwrap();
    runtime
        .mouse(&left_mouse(MouseAction::Motion, 3, 0, false))
        .unwrap();
    runtime
        .mouse(&MouseEvent {
            in_bounds: false,
            ..left_mouse(MouseAction::Release, 0, 0, false)
        })
        .unwrap();

    assert_eq!(runtime.take_clipboard_writes(), vec![b"hell".to_vec()]);
    assert!(!lock(&runtime.shared.emulator).local_selection_active);
}

#[test]
fn clipboard_writes_preserve_every_bounded_terminal_effect_in_order() {
    let runtime = TerminalRuntime::new_for_test(3, 4).unwrap();
    {
        let mut emulator = lock(&runtime.shared.emulator);
        emulator
            .terminal
            .write(b"\x1b]52;c;Zmlyc3Q=\x07\x1b]52;c;c2Vjb25k\x07");
    }

    assert_eq!(
        runtime.take_clipboard_writes(),
        vec![b"first".to_vec(), b"second".to_vec()]
    );
    assert!(runtime.take_clipboard_writes().is_empty());
}

#[test]
fn clipboard_write_fifo_rejects_overflow_without_overwriting_accepted_effects() {
    let runtime = TerminalRuntime::new_for_test(3, 4).unwrap();
    {
        let mut emulator = lock(&runtime.shared.emulator);
        emulator.terminal.write(&b"\x1b]52;c;YQ==\x07".repeat(65));
    }

    let writes = runtime.take_clipboard_writes();
    assert_eq!(writes.len(), 64);
    assert!(writes.iter().all(|write| write == b"a"));
}

#[test]
fn full_clipboard_fifo_rejects_selection_copy_without_ending_the_runtime() {
    let runtime = TerminalRuntime::new_for_test(2, 8).unwrap();
    {
        let mut emulator = lock(&runtime.shared.emulator);
        emulator.terminal.write(b"hello");
        emulator.terminal.write(&b"\x1b]52;c;YQ==\x07".repeat(64));
    }
    runtime
        .mouse(&left_mouse(MouseAction::Press, 0, 0, false))
        .unwrap();
    runtime
        .mouse(&left_mouse(MouseAction::Release, 4, 0, false))
        .unwrap();

    let writes = runtime.take_clipboard_writes();
    assert_eq!(writes.len(), 64);
    assert!(writes.iter().all(|write| write == b"a"));
    assert!(!runtime.exited());
}

#[test]
fn runtime_is_send_and_sync() {
    fn assert_traits<T: Send + Sync>() {}
    assert_traits::<TerminalRuntime>();
}

fn frame_text(runtime: &TerminalRuntime) -> String {
    runtime
        .frame()
        .unwrap()
        .cells
        .into_iter()
        .map(|cell| cell.symbol)
        .collect()
}

fn wait_for_text(runtime: &TerminalRuntime, expected: &str) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if frame_text(runtime).contains(expected) {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!(
        "terminal did not render {expected:?}: {:?}",
        frame_text(runtime)
    );
}

#[test]
fn handoff_history_never_exceeds_its_byte_limit() {
    let runtime = TerminalRuntime::new_for_test(1, 1_000).unwrap();
    let mut emulator = lock(&runtime.shared.emulator);
    emulator
        .terminal
        .write(&vec![b'x'; MAX_HANDOFF_ANSI_BYTES + 64 * 1024]);
    let total_rows = emulator.terminal.total_rows().unwrap();
    let (ansi, truncated) =
        bounded_handoff_ansi(&emulator.terminal, 1_000, total_rows, 16).unwrap();
    assert!(truncated);
    assert!(ansi.len() <= 16);
}

#[test]
fn live_handoff_preserves_process_and_pty_io() {
    let command = vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        "printf 'ready\\n'; while IFS= read -r line; do printf 'echo:%s\\n' \"$line\"; done"
            .to_string(),
    ];
    let source = TerminalRuntime::spawn(
        PaneId(41),
        TerminalId(42),
        Path::new("/"),
        &command,
        &[],
        None,
        12,
        40,
        Arc::new(|| {}),
    )
    .unwrap();
    wait_for_text(&source, "ready");
    let process_group = source.process_group_id().unwrap();

    source.pause_handoff_reader().unwrap();
    let state = source.handoff_state().unwrap();
    let fd = source.duplicate_handoff_fd().unwrap();
    let imported = unsafe { TerminalRuntime::from_handoff_fd(fd, state, Arc::new(|| {})).unwrap() };
    let source_group = source.prepare_source_handoff().unwrap();
    assert_eq!(source.process_group_id(), None);
    source.rollback_source_handoff(source_group);
    assert_eq!(source.process_group_id(), Some(source_group));
    source.prepare_source_handoff().unwrap();
    imported.assume_handoff_ownership();
    imported.activate_handoff();
    imported.write(b"after-handoff\n").unwrap();

    wait_for_text(&imported, "echo:after-handoff");
    assert_eq!(imported.process_group_id(), Some(process_group));
    assert_eq!(unsafe { libc::kill(-process_group, 0) }, 0);
    imported.terminate();
}
