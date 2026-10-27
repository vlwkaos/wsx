//! Mouse input: child reporting, local selection and selection autoscroll.
use super::*;

pub(super) const MOUSE_SCROLL_LINES: isize = 3;

// ^ vendor/libghostty-vt/include/ghostty/vt/selection.h requires embedder-driven ticks.
pub(super) const SELECTION_AUTOSCROLL_INTERVAL: Duration = Duration::from_millis(15);

#[derive(Clone, Copy)]
pub(super) struct ChildMouseGesture {
    button: MouseButton,
    last_x: u16,
    last_y: u16,
}

#[derive(Clone, Copy)]
pub(super) struct SelectionAutoscrollState {
    x: u16,
    y: u16,
}

pub(super) fn encode_mouse(
    emulator: &mut Emulator,
    mouse: &MouseEvent,
    sync_modes: bool,
) -> Result<Vec<u8>, ghostty::Error> {
    if sync_modes {
        emulator.sync_input();
    }
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
    emulator.mouse.encode(&event)
}

pub(super) fn clear_local_selection(emulator: &mut Emulator) -> Result<bool, TerminalError> {
    emulator.local_selection_active = false;
    stop_selection_autoscroll(emulator);
    emulator.child_mouse_gesture = None;
    emulator.terminal.clear_selection().map_err(Into::into)
}

pub(super) fn stop_selection_autoscroll(emulator: &mut Emulator) {
    emulator.selection_autoscroll = None;
}

pub(super) fn update_selection_autoscroll(
    emulator: &mut Emulator,
    x: u16,
    y: u16,
) -> Result<bool, TerminalError> {
    if emulator.terminal.selection_autoscroll()? == ghostty::SelectionAutoscroll::None {
        stop_selection_autoscroll(emulator);
        return Ok(false);
    }
    emulator.selection_autoscroll = Some(SelectionAutoscrollState { x, y });
    if emulator.selection_autoscroll_worker_running {
        return Ok(false);
    }
    emulator.selection_autoscroll_worker_running = true;
    Ok(true)
}

pub(super) fn spawn_selection_autoscroll(shared: Arc<Shared>) -> Result<(), TerminalError> {
    thread::Builder::new()
        .name("wsx-selection-autoscroll".into())
        .spawn(move || loop {
            thread::sleep(SELECTION_AUTOSCROLL_INTERVAL);
            let mut emulator = lock(&shared.emulator);
            let Some(state) = emulator.selection_autoscroll else {
                emulator.selection_autoscroll_worker_running = false;
                break;
            };
            let tick = (|| -> Result<(bool, bool), ghostty::Error> {
                let changed = emulator
                    .terminal
                    .selection_autoscroll_tick(state.x, state.y)?;
                let active = changed
                    && emulator.local_selection_active
                    && emulator.terminal.selection_autoscroll()?
                        != ghostty::SelectionAutoscroll::None;
                if !active {
                    stop_selection_autoscroll(&mut emulator);
                    emulator.selection_autoscroll_worker_running = false;
                }
                Ok((changed, active))
            })();
            if tick.is_err() {
                stop_selection_autoscroll(&mut emulator);
                emulator.selection_autoscroll_worker_running = false;
            }
            drop(emulator);
            match tick {
                Ok((changed, active)) => {
                    if changed {
                        shared.revision.fetch_add(1, Ordering::AcqRel);
                        (shared.notify)();
                    }
                    if !active {
                        break;
                    }
                }
                Err(error) => {
                    set_error(
                        &shared.error,
                        format!("selection autoscroll failed: {error}"),
                    );
                    (shared.notify)();
                    break;
                }
            }
        })
        .map(|_| ())
        .map_err(TerminalError::Io)
}

pub(super) fn mouse_scroll_delta(mouse: &MouseEvent) -> Option<isize> {
    if mouse.action != MouseAction::Press {
        return None;
    }
    match mouse.button {
        MouseButton::WheelUp => Some(-MOUSE_SCROLL_LINES),
        MouseButton::WheelDown => Some(MOUSE_SCROLL_LINES),
        _ => None,
    }
}

impl TerminalRuntime {
    pub fn mouse(&self, mouse: &MouseEvent) -> Result<(), TerminalError> {
        let (bytes, presentation_changed, effect_ready) = {
            let mut emulator = lock(&self.shared.emulator);
            let continuing_local = emulator.local_selection_active
                && mouse.button == MouseButton::Left
                && matches!(mouse.action, MouseAction::Motion | MouseAction::Release);
            let terminating_child = emulator.child_mouse_gesture.is_some_and(|gesture| {
                mouse.action == MouseAction::Release && mouse.button == gesture.button
            });
            if !mouse.in_bounds && !continuing_local && !terminating_child {
                return Ok(());
            }
            if mouse.in_bounds
                && (mouse.x >= emulator.terminal.cols()? || mouse.y >= emulator.terminal.rows()?)
            {
                return Err(TerminalError::Runtime(
                    "mouse coordinates exceed the terminal grid".into(),
                ));
            }
            if let Some(mut gesture) = emulator.child_mouse_gesture {
                let mut routed = *mouse;
                if mouse.in_bounds {
                    gesture.last_x = mouse.x;
                    gesture.last_y = mouse.y;
                } else {
                    routed.x = gesture.last_x;
                    routed.y = gesture.last_y;
                    routed.in_bounds = true;
                }
                let release =
                    mouse.action == MouseAction::Release && mouse.button == gesture.button;
                emulator.child_mouse_gesture = (!release).then_some(gesture);
                (encode_mouse(&mut emulator, &routed, false)?, false, false)
            } else if let Some(delta) = mouse_scroll_delta(mouse) {
                let gesture_cleared = if emulator.local_selection_active {
                    clear_local_selection(&mut emulator)?
                } else {
                    false
                };
                if !emulator.terminal.mouse_tracking_enabled()? {
                    if emulator.terminal.active_screen()? == ghostty::ActiveScreen::Alternate
                        && emulator
                            .terminal
                            .mode_get(ghostty::MODE_MOUSE_ALTERNATE_SCROLL)?
                    {
                        let code = if delta < 0 {
                            KeyCode::Up
                        } else {
                            KeyCode::Down
                        };
                        let key = KeyEvent {
                            code,
                            text: String::new(),
                            shift: false,
                            control: false,
                            alt: false,
                            super_key: false,
                            repeat: false,
                        };
                        emulator.sync_input();
                        let mut bytes = Vec::new();
                        for _ in 0..delta.unsigned_abs() {
                            bytes.extend(encode_synced_key(&mut emulator, &key)?);
                        }
                        (bytes, gesture_cleared, false)
                    } else {
                        let before = emulator.terminal.scrollbar()?.offset;
                        emulator.terminal.scroll_viewport_delta(delta);
                        let after = emulator.terminal.scrollbar()?.offset;
                        let viewport_changed = before != after;
                        let selection_cleared = if viewport_changed {
                            clear_local_selection(&mut emulator)?
                        } else {
                            false
                        };
                        (
                            Vec::new(),
                            gesture_cleared || viewport_changed || selection_cleared,
                            false,
                        )
                    }
                } else {
                    (
                        encode_mouse(&mut emulator, mouse, true)?,
                        gesture_cleared,
                        false,
                    )
                }
            } else if emulator.local_selection_active {
                match (mouse.action, mouse.button) {
                    (MouseAction::Motion, MouseButton::Left) => {
                        let boundary = if mouse.in_bounds {
                            ghostty::SelectionBoundary::Inside
                        } else if mouse.y == 0 {
                            ghostty::SelectionBoundary::Above
                        } else {
                            ghostty::SelectionBoundary::Below
                        };
                        let changed = emulator
                            .terminal
                            .selection_drag(mouse.x, mouse.y, boundary)?;
                        if update_selection_autoscroll(&mut emulator, mouse.x, mouse.y)? {
                            if let Err(error) = spawn_selection_autoscroll(Arc::clone(&self.shared))
                            {
                                stop_selection_autoscroll(&mut emulator);
                                emulator.selection_autoscroll_worker_running = false;
                                return Err(error);
                            }
                        }
                        (Vec::new(), changed, false)
                    }
                    (MouseAction::Release, MouseButton::Left) => {
                        stop_selection_autoscroll(&mut emulator);
                        let point = mouse.in_bounds.then_some((mouse.x, mouse.y));
                        let (changed, copied) = emulator.terminal.selection_release(point)?;
                        emulator.local_selection_active = false;
                        let effect_ready = copied
                            .is_some_and(|bytes| emulator.terminal.queue_clipboard_write(bytes));
                        (Vec::new(), changed, effect_ready)
                    }
                    _ => (Vec::new(), false, false),
                }
            } else {
                let tracking = emulator.terminal.mouse_tracking_enabled()?;
                if mouse.action == MouseAction::Press
                    && mouse.button == MouseButton::Left
                    && mouse.in_bounds
                    && (!tracking || mouse.shift)
                {
                    let elapsed = self.selection_clock.elapsed().as_nanos();
                    let time_ns = u64::try_from(elapsed).unwrap_or(u64::MAX);
                    let changed = emulator
                        .terminal
                        .selection_press(mouse.x, mouse.y, time_ns)?;
                    stop_selection_autoscroll(&mut emulator);
                    emulator.local_selection_active = true;
                    (Vec::new(), changed, false)
                } else {
                    let changed = if tracking && mouse.action == MouseAction::Press {
                        emulator.terminal.clear_selection()?
                    } else {
                        false
                    };
                    let bytes = encode_mouse(&mut emulator, mouse, true)?;
                    if tracking
                        && mouse.action == MouseAction::Press
                        && matches!(
                            mouse.button,
                            MouseButton::Left | MouseButton::Middle | MouseButton::Right
                        )
                    {
                        emulator.child_mouse_gesture = Some(ChildMouseGesture {
                            button: mouse.button,
                            last_x: mouse.x,
                            last_y: mouse.y,
                        });
                    }
                    (bytes, changed, false)
                }
            }
        };
        if presentation_changed {
            self.shared.revision.fetch_add(1, Ordering::AcqRel);
        }
        if presentation_changed || effect_ready {
            (self.shared.notify)();
        }
        self.write(&bytes)
    }

    pub fn clear_selection(&self) -> Result<(), TerminalError> {
        let changed = {
            let mut emulator = lock(&self.shared.emulator);
            clear_local_selection(&mut emulator)?
        };
        if changed {
            self.shared.revision.fetch_add(1, Ordering::AcqRel);
            (self.shared.notify)();
        }
        Ok(())
    }
}
