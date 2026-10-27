#[allow(
    dead_code,
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    clippy::all,
    rustdoc::all
)]
pub mod bindings;

use std::collections::VecDeque;
use std::ffi::c_void;
use std::fmt;
use std::marker::PhantomData;
use std::mem;
use std::os::raw::c_char;
use std::ptr;
use std::slice;

pub use bindings as ffi;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Error(ffi::GhosttyResult);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FormatterFormat {
    Plain,
    Vt,
}

impl FormatterFormat {
    fn as_raw(self) -> ffi::GhosttyFormatterFormat {
        match self {
            Self::Plain => ffi::GhosttyFormatterFormat_GHOSTTY_FORMATTER_FORMAT_PLAIN,
            Self::Vt => ffi::GhosttyFormatterFormat_GHOSTTY_FORMATTER_FORMAT_VT,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ghostty error {}", self.0)
    }
}

impl std::error::Error for Error {}

trait GhosttyResultExt {
    fn into_result(self) -> Result<(), Error>;
}

impl GhosttyResultExt for ffi::GhosttyResult {
    fn into_result(self) -> Result<(), Error> {
        if self == ffi::GhosttyResult_GHOSTTY_SUCCESS {
            Ok(())
        } else {
            Err(Error(self))
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dirty {
    Clean,
    Partial,
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowSelection {
    pub start_x: u16,
    pub end_x: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionBoundary {
    Inside,
    Above,
    Below,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionAutoscroll {
    None,
    Up,
    Down,
}

impl RowSelection {}

impl Dirty {
    fn from_raw(value: ffi::GhosttyRenderStateDirty) -> Self {
        match value {
            ffi::GhosttyRenderStateDirty_GHOSTTY_RENDER_STATE_DIRTY_FALSE => Self::Clean,
            ffi::GhosttyRenderStateDirty_GHOSTTY_RENDER_STATE_DIRTY_PARTIAL => Self::Partial,
            ffi::GhosttyRenderStateDirty_GHOSTTY_RENDER_STATE_DIRTY_FULL => Self::Full,
            _ => Self::Full,
        }
    }

    fn as_raw(self) -> ffi::GhosttyRenderStateDirty {
        match self {
            Self::Clean => ffi::GhosttyRenderStateDirty_GHOSTTY_RENDER_STATE_DIRTY_FALSE,
            Self::Partial => ffi::GhosttyRenderStateDirty_GHOSTTY_RENDER_STATE_DIRTY_PARTIAL,
            Self::Full => ffi::GhosttyRenderStateDirty_GHOSTTY_RENDER_STATE_DIRTY_FULL,
        }
    }
}

pub const MOD_SHIFT: u16 = ffi::GHOSTTY_MODS_SHIFT as u16;
pub const MOD_CTRL: u16 = ffi::GHOSTTY_MODS_CTRL as u16;
pub const MOD_ALT: u16 = ffi::GHOSTTY_MODS_ALT as u16;
pub const MOD_SUPER: u16 = ffi::GHOSTTY_MODS_SUPER as u16;

pub const MOUSE_ACTION_PRESS: ffi::GhosttyMouseAction =
    ffi::GhosttyMouseAction_GHOSTTY_MOUSE_ACTION_PRESS;
pub const MOUSE_ACTION_RELEASE: ffi::GhosttyMouseAction =
    ffi::GhosttyMouseAction_GHOSTTY_MOUSE_ACTION_RELEASE;
pub const MOUSE_ACTION_MOTION: ffi::GhosttyMouseAction =
    ffi::GhosttyMouseAction_GHOSTTY_MOUSE_ACTION_MOTION;
pub const MOUSE_BUTTON_LEFT: ffi::GhosttyMouseButton =
    ffi::GhosttyMouseButton_GHOSTTY_MOUSE_BUTTON_LEFT;
pub const MOUSE_BUTTON_RIGHT: ffi::GhosttyMouseButton =
    ffi::GhosttyMouseButton_GHOSTTY_MOUSE_BUTTON_RIGHT;
pub const MOUSE_BUTTON_MIDDLE: ffi::GhosttyMouseButton =
    ffi::GhosttyMouseButton_GHOSTTY_MOUSE_BUTTON_MIDDLE;
pub const MOUSE_BUTTON_WHEEL_UP: ffi::GhosttyMouseButton =
    ffi::GhosttyMouseButton_GHOSTTY_MOUSE_BUTTON_FOUR;
pub const MOUSE_BUTTON_WHEEL_DOWN: ffi::GhosttyMouseButton =
    ffi::GhosttyMouseButton_GHOSTTY_MOUSE_BUTTON_FIVE;
pub const MOUSE_BUTTON_WHEEL_LEFT: ffi::GhosttyMouseButton =
    ffi::GhosttyMouseButton_GHOSTTY_MOUSE_BUTTON_SIX;
pub const MOUSE_BUTTON_WHEEL_RIGHT: ffi::GhosttyMouseButton =
    ffi::GhosttyMouseButton_GHOSTTY_MOUSE_BUTTON_SEVEN;
pub const MODE_APPLICATION_CURSOR_KEYS: u16 = 1;
pub const MODE_FOCUS_EVENT: u16 = 1004;
pub const MODE_MOUSE_UTF8: u16 = 1005;
pub const MODE_MOUSE_SGR: u16 = 1006;
pub const MODE_MOUSE_ALTERNATE_SCROLL: u16 = 1007;
pub const MODE_MOUSE_SGR_PIXELS: u16 = 1016;
pub const MODE_BRACKETED_PASTE: u16 = 2004;
pub const MODE_SYNCHRONIZED_OUTPUT: u16 = 2026;
pub const MODE_GRAPHEME_CLUSTER: u16 = 2027;
pub const MODE_COLOR_SCHEME_REPORT: u16 = 2031;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorVisualStyle {
    Bar,
    Block,
    Underline,
    BlockHollow,
}

impl CursorVisualStyle {
    fn from_raw(value: ffi::GhosttyRenderStateCursorVisualStyle) -> Self {
        match value {
            ffi::GhosttyRenderStateCursorVisualStyle_GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BLOCK => {
                Self::Block
            }
            ffi::GhosttyRenderStateCursorVisualStyle_GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_UNDERLINE => {
                Self::Underline
            }
            ffi::GhosttyRenderStateCursorVisualStyle_GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BLOCK_HOLLOW => {
                Self::BlockHollow
            }
            _ => Self::Bar,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveScreen {
    Primary,
    Alternate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalScrollbar {
    pub total: usize,
    pub offset: usize,
    pub len: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorViewport {
    pub x: u16,
    pub y: u16,
    pub wide_tail: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RgbColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl From<ffi::GhosttyColorRgb> for RgbColor {
    fn from(value: ffi::GhosttyColorRgb) -> Self {
        Self {
            r: value.r,
            g: value.g,
            b: value.b,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellColor {
    Palette(u8),
    Rgb(RgbColor),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CellStyle {
    pub fg_color: Option<CellColor>,
    pub bg_color: Option<CellColor>,
    pub underline_color: Option<CellColor>,
    pub bold: bool,
    pub italic: bool,
    pub faint: bool,
    pub blink: bool,
    pub inverse: bool,
    pub invisible: bool,
    pub strikethrough: bool,
    pub overline: bool,
    pub underline: u8,
    pub underlined: bool,
}

impl From<ffi::GhosttyStyle> for CellStyle {
    fn from(value: ffi::GhosttyStyle) -> Self {
        Self {
            fg_color: cell_color_from_style_color(value.fg_color),
            bg_color: cell_color_from_style_color(value.bg_color),
            underline_color: cell_color_from_style_color(value.underline_color),
            bold: value.bold,
            italic: value.italic,
            faint: value.faint,
            blink: value.blink,
            inverse: value.inverse,
            invisible: value.invisible,
            strikethrough: value.strikethrough,
            overline: value.overline,
            underline: normalize_underline_style(value.underline),
            underlined: value.underline != 0,
        }
    }
}

fn normalize_underline_style(value: std::os::raw::c_int) -> u8 {
    match value {
        0..=5 => value as u8,
        _ => 1,
    }
}

fn cell_color_from_style_color(color: ffi::GhosttyStyleColor) -> Option<CellColor> {
    match color.tag {
        ffi::GhosttyStyleColorTag_GHOSTTY_STYLE_COLOR_PALETTE => {
            // SAFETY: Ghostty's tagged union stores `palette` when the tag is PALETTE.
            Some(CellColor::Palette(unsafe { color.value.palette }))
        }
        ffi::GhosttyStyleColorTag_GHOSTTY_STYLE_COLOR_RGB => {
            // SAFETY: Ghostty's tagged union stores `rgb` when the tag is RGB.
            Some(CellColor::Rgb(unsafe { color.value.rgb }.into()))
        }
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderColors {
    pub background: RgbColor,
    pub foreground: RgbColor,
    pub palette: [RgbColor; 256],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellWide {
    Narrow,
    Wide,
    SpacerTail,
    SpacerHead,
}

impl CellWide {
    fn from_raw(value: ffi::GhosttyCellWide) -> Self {
        match value {
            ffi::GhosttyCellWide_GHOSTTY_CELL_WIDE_NARROW => Self::Narrow,
            ffi::GhosttyCellWide_GHOSTTY_CELL_WIDE_WIDE => Self::Wide,
            ffi::GhosttyCellWide_GHOSTTY_CELL_WIDE_SPACER_TAIL => Self::SpacerTail,
            ffi::GhosttyCellWide_GHOSTTY_CELL_WIDE_SPACER_HEAD => Self::SpacerHead,
            _ => Self::Narrow,
        }
    }
}

type WritePtyCallback = dyn FnMut(&[u8]) + Send;

const MAX_CLIPBOARD_BYTES: usize = 192 * 1024;
const MAX_PENDING_CLIPBOARD_WRITES: usize = 64;

#[derive(Default)]
struct TerminalCallbackState {
    write_pty: Option<Box<WritePtyCallback>>,
    bell_count: u16,
    pwd_changes: Vec<Vec<u8>>,
    clipboard_writes: VecDeque<Vec<u8>>,
    size_report: ffi::GhosttySizeReportSize,
}

unsafe extern "C" fn bell_trampoline(_terminal: ffi::GhosttyTerminal, userdata: *mut c_void) {
    if userdata.is_null() {
        return;
    }
    // SAFETY: userdata is the TerminalCallbackState installed with this terminal.
    let state = unsafe { &mut *userdata.cast::<TerminalCallbackState>() };
    state.bell_count = state.bell_count.saturating_add(1);
}

unsafe extern "C" fn color_scheme_trampoline(
    _terminal: ffi::GhosttyTerminal,
    _userdata: *mut c_void,
    _out_scheme: *mut ffi::GhosttyColorScheme,
) -> bool {
    // No color scheme is ever reported; the terminal falls back to its default.
    false
}

unsafe extern "C" fn size_trampoline(
    _terminal: ffi::GhosttyTerminal,
    userdata: *mut c_void,
    out_size: *mut ffi::GhosttySizeReportSize,
) -> bool {
    if userdata.is_null() || out_size.is_null() {
        return false;
    }
    let state = unsafe { &*userdata.cast::<TerminalCallbackState>() };
    let size = state.size_report;
    if size.rows == 0 || size.columns == 0 || size.cell_width == 0 || size.cell_height == 0 {
        return false;
    }
    unsafe {
        out_size.write(size);
    }
    true
}

unsafe extern "C" fn write_pty_trampoline(
    _terminal: ffi::GhosttyTerminal,
    userdata: *mut c_void,
    data: *const u8,
    len: usize,
) {
    if userdata.is_null() || (data.is_null() && len != 0) {
        return;
    }
    let state = unsafe { &mut *(userdata.cast::<TerminalCallbackState>()) };
    let Some(callback) = state.write_pty.as_mut() else {
        return;
    };
    let bytes = if len == 0 {
        &[]
    } else {
        unsafe { slice::from_raw_parts(data, len) }
    };
    callback(bytes);
}

unsafe extern "C" fn clipboard_write_trampoline(
    _terminal: ffi::GhosttyTerminal,
    userdata: *mut c_void,
    write: *const ffi::GhosttyClipboardWrite,
) -> ffi::GhosttyClipboardWriteResult {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // SAFETY: libghostty-vt owns these values for the synchronous callback.
        unsafe { capture_clipboard_write(userdata, write) }
    }))
    .unwrap_or(ffi::GhosttyClipboardWriteResult_GHOSTTY_CLIPBOARD_WRITE_RESULT_INVALID_DATA)
}

unsafe fn capture_clipboard_write(
    userdata: *mut c_void,
    write: *const ffi::GhosttyClipboardWrite,
) -> ffi::GhosttyClipboardWriteResult {
    if userdata.is_null() || write.is_null() {
        return ffi::GhosttyClipboardWriteResult_GHOSTTY_CLIPBOARD_WRITE_RESULT_INVALID_DATA;
    }

    let required_size = std::mem::offset_of!(ffi::GhosttyClipboardWrite, contents_len)
        + std::mem::size_of::<usize>();
    // SAFETY: size is the leading field of the live request.
    if unsafe { (*write).size } < required_size {
        return ffi::GhosttyClipboardWriteResult_GHOSTTY_CLIPBOARD_WRITE_RESULT_INVALID_DATA;
    }
    // SAFETY: the size check covers every field accessed below.
    let request = unsafe { &*write };
    if request.location != ffi::GhosttyClipboardLocation_GHOSTTY_CLIPBOARD_LOCATION_STANDARD {
        return ffi::GhosttyClipboardWriteResult_GHOSTTY_CLIPBOARD_WRITE_RESULT_UNSUPPORTED;
    }

    // SAFETY: userdata is the TerminalCallbackState installed with this terminal.
    let state = unsafe { &mut *userdata.cast::<TerminalCallbackState>() };
    if request.contents_len == 0 {
        return ffi::GhosttyClipboardWriteResult_GHOSTTY_CLIPBOARD_WRITE_RESULT_SUCCESS;
    }
    if request.contents_len != 1 {
        return ffi::GhosttyClipboardWriteResult_GHOSTTY_CLIPBOARD_WRITE_RESULT_UNSUPPORTED;
    }
    if request.contents.is_null() {
        return ffi::GhosttyClipboardWriteResult_GHOSTTY_CLIPBOARD_WRITE_RESULT_INVALID_DATA;
    }

    // SAFETY: libghostty-vt keeps the single content and its strings alive for the callback.
    let content = unsafe { &*request.contents };
    // SAFETY: the MIME string is borrowed from the live callback request.
    let Some(mime) = (unsafe { borrowed_bytes(content.mime) }) else {
        return ffi::GhosttyClipboardWriteResult_GHOSTTY_CLIPBOARD_WRITE_RESULT_INVALID_DATA;
    };
    let is_text = std::str::from_utf8(mime)
        .ok()
        .and_then(|mime| mime.split(';').next())
        .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("text/plain"));
    if !is_text {
        return ffi::GhosttyClipboardWriteResult_GHOSTTY_CLIPBOARD_WRITE_RESULT_UNSUPPORTED;
    }

    // SAFETY: the data string is borrowed from the live callback request.
    let Some(bytes) = (unsafe { borrowed_bytes(content.data) }) else {
        return ffi::GhosttyClipboardWriteResult_GHOSTTY_CLIPBOARD_WRITE_RESULT_INVALID_DATA;
    };
    if bytes.is_empty() {
        return ffi::GhosttyClipboardWriteResult_GHOSTTY_CLIPBOARD_WRITE_RESULT_UNSUPPORTED;
    }
    if bytes.len() > MAX_CLIPBOARD_BYTES
        || state.clipboard_writes.len() >= MAX_PENDING_CLIPBOARD_WRITES
    {
        return ffi::GhosttyClipboardWriteResult_GHOSTTY_CLIPBOARD_WRITE_RESULT_INVALID_DATA;
    }
    state.clipboard_writes.push_back(bytes.to_vec());
    ffi::GhosttyClipboardWriteResult_GHOSTTY_CLIPBOARD_WRITE_RESULT_SUCCESS
}

unsafe fn borrowed_bytes<'a>(value: ffi::GhosttyString) -> Option<&'a [u8]> {
    if value.len == 0 {
        Some(&[])
    } else if value.ptr.is_null() {
        None
    } else {
        // SAFETY: the callback contract keeps pointer and length valid until return.
        Some(unsafe { slice::from_raw_parts(value.ptr, value.len) })
    }
}

unsafe extern "C" fn pwd_changed_trampoline(terminal: ffi::GhosttyTerminal, userdata: *mut c_void) {
    if terminal.is_null() || userdata.is_null() {
        return;
    }
    let mut pwd = ffi::GhosttyString::default();
    let result = unsafe {
        ffi::ghostty_terminal_get(
            terminal,
            ffi::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_PWD,
            (&mut pwd as *mut ffi::GhosttyString).cast(),
        )
    };
    if result != ffi::GhosttyResult_GHOSTTY_SUCCESS || (pwd.ptr.is_null() && pwd.len != 0) {
        return;
    }
    let bytes = if pwd.len == 0 {
        Vec::new()
    } else {
        unsafe { slice::from_raw_parts(pwd.ptr, pwd.len) }.to_vec()
    };
    let state = unsafe { &mut *(userdata.cast::<TerminalCallbackState>()) };
    state.pwd_changes.push(bytes);
}

pub fn encode_paste(data: &[u8], bracketed: bool) -> Result<Vec<u8>, Error> {
    let mut input = data.to_vec();
    let mut required = 0usize;
    // SAFETY: null output buffer queries the required size; input owns data_len bytes.
    let result = unsafe {
        ffi::ghostty_paste_encode(
            input.as_mut_ptr().cast(),
            input.len(),
            bracketed,
            ptr::null_mut(),
            0,
            &mut required,
        )
    };
    if result != ffi::GhosttyResult_GHOSTTY_OUT_OF_SPACE {
        result.into_result()?;
    }
    let mut input = data.to_vec();
    let mut output = vec![0u8; required];
    // SAFETY: input and output own their declared lengths and do not overlap.
    unsafe {
        ffi::ghostty_paste_encode(
            input.as_mut_ptr().cast(),
            input.len(),
            bracketed,
            output.as_mut_ptr().cast(),
            output.len(),
            &mut required,
        )
        .into_result()?;
    }
    output.truncate(required);
    Ok(output)
}

pub struct Terminal {
    raw: ffi::GhosttyTerminal,
    selection_gesture: ffi::GhosttySelectionGesture,
    #[cfg(windows)]
    tracked_row: ffi::GhosttyTrackedGridRef,
    callback_state: Box<TerminalCallbackState>,
}

impl Terminal {
    pub fn new(cols: u16, rows: u16, max_scrollback: usize) -> Result<Self, Error> {
        let mut raw = ptr::null_mut();
        let options = ffi::GhosttyTerminalOptions {
            cols,
            rows,
            max_scrollback,
        };
        // SAFETY: valid out pointer and options, null allocator means default allocator.
        unsafe {
            ffi::ghostty_terminal_new(ptr::null(), &mut raw, options).into_result()?;
        }

        let mut selection_gesture = ptr::null_mut();
        let gesture_result =
            unsafe { ffi::ghostty_selection_gesture_new(ptr::null(), &mut selection_gesture) };
        if let Err(error) = gesture_result.into_result() {
            unsafe { ffi::ghostty_terminal_free(raw) };
            return Err(error);
        }

        let mut terminal = Self {
            raw,
            selection_gesture,
            #[cfg(windows)]
            tracked_row: ptr::null_mut(),
            callback_state: Box::new(TerminalCallbackState {
                size_report: ffi::GhosttySizeReportSize {
                    rows,
                    columns: cols,
                    ..Default::default()
                },
                ..Default::default()
            }),
        };
        let userdata = (&mut *terminal.callback_state as *mut TerminalCallbackState).cast();
        let glyph_protocol = false;
        unsafe {
            ffi::ghostty_terminal_set(
                terminal.raw,
                ffi::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_USERDATA,
                userdata,
            )
            .into_result()?;
            ffi::ghostty_terminal_set(
                terminal.raw,
                ffi::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_SIZE,
                (size_trampoline as *const ()).cast(),
            )
            .into_result()?;
            ffi::ghostty_terminal_set(
                terminal.raw,
                ffi::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_BELL,
                (bell_trampoline as *const ()).cast(),
            )
            .into_result()?;
            ffi::ghostty_terminal_set(
                terminal.raw,
                ffi::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_PWD_CHANGED,
                (pwd_changed_trampoline as *const ()).cast(),
            )
            .into_result()?;
            ffi::ghostty_terminal_set(
                terminal.raw,
                ffi::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_CLIPBOARD_WRITE,
                (clipboard_write_trampoline as *const ()).cast(),
            )
            .into_result()?;
            ffi::ghostty_terminal_set(
                terminal.raw,
                ffi::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_COLOR_SCHEME,
                (color_scheme_trampoline as *const ()).cast(),
            )
            .into_result()?;
            ffi::ghostty_terminal_set(
                terminal.raw,
                ffi::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_GLYPH_PROTOCOL,
                (&glyph_protocol as *const bool).cast(),
            )
            .into_result()?;
        }
        Ok(terminal)
    }

    pub fn write(&mut self, bytes: &[u8]) {
        let screen_before = self.active_screen().ok();
        // SAFETY: self.raw is a live terminal handle for self's lifetime.
        unsafe {
            ffi::ghostty_terminal_vt_write(self.raw, bytes.as_ptr(), bytes.len());
        }
        let screen_after = self.active_screen().ok();
        if screen_before
            .zip(screen_after)
            .is_some_and(|(before, after)| before != after)
        {
            // ^ Selection grid references must not cross primary/alternate screen identity.
            let _ = self.clear_selection();
        }
    }

    pub fn resize(
        &mut self,
        cols: u16,
        rows: u16,
        cell_width_px: u32,
        cell_height_px: u32,
    ) -> Result<(), Error> {
        let size_report = ffi::GhosttySizeReportSize {
            rows,
            columns: cols,
            cell_width: cell_width_px,
            cell_height: cell_height_px,
        };
        // SAFETY: self.raw is valid and sizes are plain values.
        unsafe {
            ffi::ghostty_terminal_resize(
                self.raw,
                cols,
                rows,
                cell_width_px.max(1),
                cell_height_px.max(1),
            )
            .into_result()?;
        }
        self.callback_state.size_report = size_report;
        Ok(())
    }

    pub fn set_write_pty_callback<F>(&mut self, callback: F) -> Result<(), Error>
    where
        F: FnMut(&[u8]) + Send + 'static,
    {
        unsafe {
            ffi::ghostty_terminal_set(
                self.raw,
                ffi::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_WRITE_PTY,
                (write_pty_trampoline as *const ()).cast(),
            )
            .into_result()?;
        }
        self.callback_state.write_pty = Some(Box::new(callback));
        Ok(())
    }

    pub fn take_clipboard_writes(&mut self) -> Vec<Vec<u8>> {
        self.callback_state.clipboard_writes.drain(..).collect()
    }

    pub fn queue_clipboard_write(&mut self, bytes: Vec<u8>) -> bool {
        if bytes.is_empty()
            || bytes.len() > MAX_CLIPBOARD_BYTES
            || self.callback_state.clipboard_writes.len() >= MAX_PENDING_CLIPBOARD_WRITES
        {
            return false;
        }
        self.callback_state.clipboard_writes.push_back(bytes);
        true
    }

    pub fn selection_press(&mut self, x: u16, y: u16, time_ns: u64) -> Result<bool, Error> {
        let grid_ref = self.grid_ref(ghostty_viewport_point(x, u32::from(y)))?;
        let position = selection_position(x, y);
        let repeat_distance = f64::from(SELECTION_CELL_SIZE);
        let repeat_interval_ns = 500_000_000u64;
        let behaviors = ffi::GhosttySelectionGestureBehaviors {
            single_click:
                ffi::GhosttySelectionGestureBehavior_GHOSTTY_SELECTION_GESTURE_BEHAVIOR_CELL,
            double_click:
                ffi::GhosttySelectionGestureBehavior_GHOSTTY_SELECTION_GESTURE_BEHAVIOR_WORD,
            triple_click:
                ffi::GhosttySelectionGestureBehavior_GHOSTTY_SELECTION_GESTURE_BEHAVIOR_LINE,
        };
        let event = SelectionGestureEvent::new(
            ffi::GhosttySelectionGestureEventType_GHOSTTY_SELECTION_GESTURE_EVENT_TYPE_PRESS,
        )?;
        event.set(
            ffi::GhosttySelectionGestureEventOption_GHOSTTY_SELECTION_GESTURE_EVENT_OPT_REF,
            &grid_ref,
        )?;
        event.set(
            ffi::GhosttySelectionGestureEventOption_GHOSTTY_SELECTION_GESTURE_EVENT_OPT_POSITION,
            &position,
        )?;
        event.set(
            ffi::GhosttySelectionGestureEventOption_GHOSTTY_SELECTION_GESTURE_EVENT_OPT_REPEAT_DISTANCE,
            &repeat_distance,
        )?;
        event.set(
            ffi::GhosttySelectionGestureEventOption_GHOSTTY_SELECTION_GESTURE_EVENT_OPT_TIME_NS,
            &time_ns,
        )?;
        event.set(
            ffi::GhosttySelectionGestureEventOption_GHOSTTY_SELECTION_GESTURE_EVENT_OPT_REPEAT_INTERVAL_NS,
            &repeat_interval_ns,
        )?;
        event.set(
            ffi::GhosttySelectionGestureEventOption_GHOSTTY_SELECTION_GESTURE_EVENT_OPT_BEHAVIORS,
            &behaviors,
        )?;
        let selection = self.apply_selection_event(&event)?;
        self.install_selection(selection.as_ref())
    }

    pub fn selection_drag(
        &mut self,
        x: u16,
        y: u16,
        boundary: SelectionBoundary,
    ) -> Result<bool, Error> {
        let grid_ref = self.grid_ref(ghostty_viewport_point(x, u32::from(y)))?;
        let position = self.selection_position_at_boundary(x, y, boundary);
        let geometry = self.selection_geometry();
        let event = SelectionGestureEvent::new(
            ffi::GhosttySelectionGestureEventType_GHOSTTY_SELECTION_GESTURE_EVENT_TYPE_DRAG,
        )?;
        event.set(
            ffi::GhosttySelectionGestureEventOption_GHOSTTY_SELECTION_GESTURE_EVENT_OPT_REF,
            &grid_ref,
        )?;
        event.set(
            ffi::GhosttySelectionGestureEventOption_GHOSTTY_SELECTION_GESTURE_EVENT_OPT_POSITION,
            &position,
        )?;
        event.set(
            ffi::GhosttySelectionGestureEventOption_GHOSTTY_SELECTION_GESTURE_EVENT_OPT_GEOMETRY,
            &geometry,
        )?;
        let selection = self.apply_selection_event(&event)?;
        let selection = self.cell_selection_or(selection, grid_ref)?;
        self.install_selection(selection.as_ref())
    }

    pub fn selection_autoscroll_tick(&mut self, x: u16, y: u16) -> Result<bool, Error> {
        let before = self.scrollbar()?.offset;
        let boundary = match self.selection_autoscroll()? {
            SelectionAutoscroll::Up => SelectionBoundary::Above,
            SelectionAutoscroll::Down => SelectionBoundary::Below,
            SelectionAutoscroll::None => return Ok(false),
        };
        let viewport = ffi::GhosttyPointCoordinate { x, y: u32::from(y) };
        let position = self.selection_position_at_boundary(x, y, boundary);
        let geometry = self.selection_geometry();
        let event = SelectionGestureEvent::new(
            ffi::GhosttySelectionGestureEventType_GHOSTTY_SELECTION_GESTURE_EVENT_TYPE_AUTOSCROLL_TICK,
        )?;
        event.set(
            ffi::GhosttySelectionGestureEventOption_GHOSTTY_SELECTION_GESTURE_EVENT_OPT_VIEWPORT,
            &viewport,
        )?;
        event.set(
            ffi::GhosttySelectionGestureEventOption_GHOSTTY_SELECTION_GESTURE_EVENT_OPT_POSITION,
            &position,
        )?;
        event.set(
            ffi::GhosttySelectionGestureEventOption_GHOSTTY_SELECTION_GESTURE_EVENT_OPT_GEOMETRY,
            &geometry,
        )?;
        let selection = self.apply_selection_event(&event)?;
        if selection.is_none() && self.selection_autoscroll()? == SelectionAutoscroll::None {
            return Ok(before != self.scrollbar()?.offset);
        }
        let grid_ref = self.grid_ref(ghostty_viewport_point(x, u32::from(y)))?;
        let selection = self.cell_selection_or(selection, grid_ref)?;
        let selection_changed = self.install_selection(selection.as_ref())?;
        Ok(before != self.scrollbar()?.offset || selection_changed)
    }

    pub fn selection_autoscroll(&self) -> Result<SelectionAutoscroll, Error> {
        let mut direction =
            ffi::GhosttySelectionGestureAutoscroll_GHOSTTY_SELECTION_GESTURE_AUTOSCROLL_NONE;
        unsafe {
            ffi::ghostty_selection_gesture_get(
                self.selection_gesture,
                self.raw,
                ffi::GhosttySelectionGestureData_GHOSTTY_SELECTION_GESTURE_DATA_AUTOSCROLL,
                (&mut direction as *mut ffi::GhosttySelectionGestureAutoscroll).cast(),
            )
            .into_result()?;
        }
        Ok(match direction {
            ffi::GhosttySelectionGestureAutoscroll_GHOSTTY_SELECTION_GESTURE_AUTOSCROLL_UP => {
                SelectionAutoscroll::Up
            }
            ffi::GhosttySelectionGestureAutoscroll_GHOSTTY_SELECTION_GESTURE_AUTOSCROLL_DOWN => {
                SelectionAutoscroll::Down
            }
            _ => SelectionAutoscroll::None,
        })
    }

    fn selection_geometry(&self) -> ffi::GhosttySelectionGestureGeometry {
        ffi::GhosttySelectionGestureGeometry {
            columns: u32::from(self.callback_state.size_report.columns.max(1)),
            cell_width: SELECTION_CELL_SIZE,
            padding_left: 0,
            screen_height: u32::from(self.callback_state.size_report.rows.max(1))
                * SELECTION_CELL_SIZE,
        }
    }

    fn selection_position_at_boundary(
        &self,
        x: u16,
        y: u16,
        boundary: SelectionBoundary,
    ) -> ffi::GhosttySurfacePosition {
        let mut position = selection_position(x, y);
        position.y = match boundary {
            SelectionBoundary::Inside => position.y,
            SelectionBoundary::Above => 0.0,
            SelectionBoundary::Below => f64::from(self.selection_geometry().screen_height),
        };
        position
    }

    fn cell_selection_or(
        &self,
        selection: Option<ffi::GhosttySelection>,
        endpoint: ffi::GhosttyGridRef,
    ) -> Result<Option<ffi::GhosttySelection>, Error> {
        if self.selection_gesture_behavior()?
            == ffi::GhosttySelectionGestureBehavior_GHOSTTY_SELECTION_GESTURE_BEHAVIOR_CELL
        {
            Ok(Some(ffi::GhosttySelection {
                size: mem::size_of::<ffi::GhosttySelection>(),
                start: self.selection_gesture_anchor()?,
                end: endpoint,
                rectangle: false,
            }))
        } else {
            Ok(selection)
        }
    }

    pub fn selection_release(
        &mut self,
        point: Option<(u16, u16)>,
    ) -> Result<(bool, Option<Vec<u8>>), Error> {
        let grid_ref = point
            .map(|(x, y)| self.grid_ref(ghostty_viewport_point(x, u32::from(y))))
            .transpose()?;
        let anchor = self.selection_gesture_anchor()?;
        let behavior = self.selection_gesture_behavior()?;
        let collapsed_cell = grid_ref.as_ref().is_some_and(|target| {
            behavior == ffi::GhosttySelectionGestureBehavior_GHOSTTY_SELECTION_GESTURE_BEHAVIOR_CELL
                && same_grid_ref(&anchor, target)
        });
        let mut changed = match (point, grid_ref.as_ref()) {
            (Some((x, y)), Some(target)) if collapsed_cell || !same_grid_ref(&anchor, target) => {
                self.selection_drag(x, y, SelectionBoundary::Inside)?
            }
            _ => false,
        };
        let event = SelectionGestureEvent::new(
            ffi::GhosttySelectionGestureEventType_GHOSTTY_SELECTION_GESTURE_EVENT_TYPE_RELEASE,
        )?;
        if let Some(grid_ref) = grid_ref.as_ref() {
            event.set(
                ffi::GhosttySelectionGestureEventOption_GHOSTTY_SELECTION_GESTURE_EVENT_OPT_REF,
                grid_ref,
            )?;
        }
        let _ = self.apply_selection_event(&event)?;
        if collapsed_cell {
            changed |= self.install_selection(None)?;
            return Ok((changed, None));
        }
        Ok((changed, self.selection_text()?))
    }

    pub fn clear_selection(&mut self) -> Result<bool, Error> {
        unsafe { ffi::ghostty_selection_gesture_reset(self.selection_gesture, self.raw) };
        self.install_selection(None)
    }

    fn apply_selection_event(
        &mut self,
        event: &SelectionGestureEvent,
    ) -> Result<Option<ffi::GhosttySelection>, Error> {
        let mut selection = ffi::GhosttySelection {
            size: mem::size_of::<ffi::GhosttySelection>(),
            ..Default::default()
        };
        let result = unsafe {
            ffi::ghostty_selection_gesture_event(
                self.selection_gesture,
                self.raw,
                event.raw,
                &mut selection,
            )
        };
        match result {
            ffi::GhosttyResult_GHOSTTY_SUCCESS => Ok(Some(selection)),
            ffi::GhosttyResult_GHOSTTY_NO_VALUE => Ok(None),
            other => Err(Error(other)),
        }
    }

    fn selection_gesture_behavior(&self) -> Result<ffi::GhosttySelectionGestureBehavior, Error> {
        let mut behavior =
            ffi::GhosttySelectionGestureBehavior_GHOSTTY_SELECTION_GESTURE_BEHAVIOR_CELL;
        unsafe {
            ffi::ghostty_selection_gesture_get(
                self.selection_gesture,
                self.raw,
                ffi::GhosttySelectionGestureData_GHOSTTY_SELECTION_GESTURE_DATA_BEHAVIOR,
                (&mut behavior as *mut ffi::GhosttySelectionGestureBehavior).cast(),
            )
            .into_result()?;
        }
        Ok(behavior)
    }

    fn selection_gesture_anchor(&self) -> Result<ffi::GhosttyGridRef, Error> {
        let mut anchor = ffi::GhosttyGridRef::default();
        unsafe {
            ffi::ghostty_selection_gesture_get(
                self.selection_gesture,
                self.raw,
                ffi::GhosttySelectionGestureData_GHOSTTY_SELECTION_GESTURE_DATA_ANCHOR,
                (&mut anchor as *mut ffi::GhosttyGridRef).cast(),
            )
            .into_result()?;
        }
        Ok(anchor)
    }

    fn current_selection(&self) -> Result<Option<ffi::GhosttySelection>, Error> {
        let mut selection = ffi::GhosttySelection {
            size: mem::size_of::<ffi::GhosttySelection>(),
            ..Default::default()
        };
        let result = unsafe {
            ffi::ghostty_terminal_get(
                self.raw,
                ffi::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_SELECTION,
                (&mut selection as *mut ffi::GhosttySelection).cast(),
            )
        };
        match result {
            ffi::GhosttyResult_GHOSTTY_SUCCESS => Ok(Some(selection)),
            ffi::GhosttyResult_GHOSTTY_NO_VALUE => Ok(None),
            other => Err(Error(other)),
        }
    }

    fn install_selection(
        &mut self,
        selection: Option<&ffi::GhosttySelection>,
    ) -> Result<bool, Error> {
        let current = self.current_selection()?;
        let changed = match (current.as_ref(), selection) {
            (None, None) => false,
            (Some(_), None) | (None, Some(_)) => true,
            (Some(current), Some(selection)) => {
                let mut equal = false;
                unsafe {
                    ffi::ghostty_terminal_selection_equal(self.raw, current, selection, &mut equal)
                        .into_result()?;
                }
                !equal
            }
        };
        if !changed {
            return Ok(false);
        }
        let selection_ptr = selection.map_or(ptr::null(), |selection| selection);
        unsafe {
            ffi::ghostty_terminal_set(
                self.raw,
                ffi::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_SELECTION,
                selection_ptr.cast(),
            )
            .into_result()?;
        }
        Ok(true)
    }

    fn selection_text(&self) -> Result<Option<Vec<u8>>, Error> {
        let options = ffi::GhosttyTerminalSelectionFormatOptions {
            size: mem::size_of::<ffi::GhosttyTerminalSelectionFormatOptions>(),
            emit: FormatterFormat::Plain.as_raw(),
            unwrap: true,
            trim: true,
            selection: ptr::null(),
        };
        let mut out_ptr = ptr::null_mut();
        let mut out_len = 0usize;
        let result = unsafe {
            ffi::ghostty_terminal_selection_format_alloc(
                self.raw,
                ptr::null(),
                options,
                &mut out_ptr,
                &mut out_len,
            )
        };
        match result {
            ffi::GhosttyResult_GHOSTTY_NO_VALUE => return Ok(None),
            ffi::GhosttyResult_GHOSTTY_SUCCESS => {}
            other => return Err(Error(other)),
        }
        let bytes = if out_len == 0 {
            Vec::new()
        } else {
            unsafe { slice::from_raw_parts(out_ptr.cast_const(), out_len) }.to_vec()
        };
        if !out_ptr.is_null() {
            unsafe { ffi::ghostty_free(ptr::null(), out_ptr, out_len) };
        }
        Ok((!bytes.is_empty()).then_some(bytes))
    }

    pub fn mode_get(&self, mode: u16) -> Result<bool, Error> {
        let mut out = false;
        unsafe { ffi::ghostty_terminal_mode_get(self.raw, mode, &mut out).into_result()? };
        Ok(out)
    }

    pub fn mode_set(&mut self, mode: u16, value: bool) -> Result<(), Error> {
        unsafe { ffi::ghostty_terminal_mode_set(self.raw, mode, value).into_result() }
    }

    pub fn kitty_keyboard_flags(&self) -> Result<u8, Error> {
        let mut out = 0u8;
        unsafe {
            ffi::ghostty_terminal_get(
                self.raw,
                ffi::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_KITTY_KEYBOARD_FLAGS,
                (&mut out as *mut u8).cast(),
            )
            .into_result()?;
        }
        Ok(out)
    }

    pub fn mouse_tracking_enabled(&self) -> Result<bool, Error> {
        self.get_bool(ffi::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_MOUSE_TRACKING)
    }

    pub fn active_screen(&self) -> Result<ActiveScreen, Error> {
        let mut out = ffi::GhosttyTerminalScreen_GHOSTTY_TERMINAL_SCREEN_PRIMARY;
        unsafe {
            ffi::ghostty_terminal_get(
                self.raw,
                ffi::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_ACTIVE_SCREEN,
                (&mut out as *mut ffi::GhosttyTerminalScreen).cast(),
            )
            .into_result()?;
        }
        Ok(match out {
            ffi::GhosttyTerminalScreen_GHOSTTY_TERMINAL_SCREEN_PRIMARY => ActiveScreen::Primary,
            ffi::GhosttyTerminalScreen_GHOSTTY_TERMINAL_SCREEN_ALTERNATE => ActiveScreen::Alternate,
            _ => ActiveScreen::Primary,
        })
    }

    pub fn total_rows(&self) -> Result<usize, Error> {
        self.get_usize(ffi::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_TOTAL_ROWS)
    }

    pub fn scrollbar(&self) -> Result<TerminalScrollbar, Error> {
        let mut out = ffi::GhosttyTerminalScrollbar::default();
        unsafe {
            ffi::ghostty_terminal_get(
                self.raw,
                ffi::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_SCROLLBAR,
                (&mut out as *mut ffi::GhosttyTerminalScrollbar).cast(),
            )
            .into_result()?;
        }
        Ok(TerminalScrollbar {
            total: out.total as usize,
            offset: out.offset as usize,
            len: out.len as usize,
        })
    }

    #[cfg(windows)]
    pub(crate) fn track_row(&mut self, y: u32) -> Option<usize> {
        let mut point = ffi::GhosttyPointCoordinate::default();
        let tag = ffi::GhosttyPointTag_GHOSTTY_POINT_TAG_SCREEN;
        let result =
            unsafe { ffi::ghostty_tracked_grid_ref_point(self.tracked_row, tag, &mut point) };
        let terminal = self.raw;
        let target = ghostty_viewport_point(0, y);
        unsafe {
            ffi::ghostty_tracked_grid_ref_free(self.tracked_row);
            self.tracked_row = ptr::null_mut();
            let _ = ffi::ghostty_terminal_grid_ref_track(terminal, target, &mut self.tracked_row);
        }
        (result == ffi::GhosttyResult_GHOSTTY_SUCCESS).then_some(point.y as usize)
    }

    pub fn title(&self) -> Result<String, Error> {
        let mut title = ffi::GhosttyString::default();
        unsafe {
            ffi::ghostty_terminal_get(
                self.raw,
                ffi::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_TITLE,
                (&mut title as *mut ffi::GhosttyString).cast(),
            )
            .into_result()?;
        }
        if title.ptr.is_null() && title.len != 0 {
            return Err(Error(ffi::GhosttyResult_GHOSTTY_INVALID_VALUE));
        }
        let bytes = if title.len == 0 {
            &[]
        } else {
            unsafe { slice::from_raw_parts(title.ptr, title.len) }
        };
        Ok(String::from_utf8_lossy(bytes).into_owned())
    }

    fn grid_ref(&self, point: ffi::GhosttyPoint) -> Result<ffi::GhosttyGridRef, Error> {
        let mut grid_ref = ffi::GhosttyGridRef {
            size: mem::size_of::<ffi::GhosttyGridRef>(),
            ..Default::default()
        };
        unsafe {
            ffi::ghostty_terminal_grid_ref(self.raw, point, &mut grid_ref).into_result()?;
        }
        Ok(grid_ref)
    }

    pub fn read_text_screen(
        &self,
        start: (u16, u32),
        end: (u16, u32),
        rectangle: bool,
    ) -> Result<String, Error> {
        self.read_formatted_selection(
            ghostty_screen_point(start.0, start.1),
            ghostty_screen_point(end.0, end.1),
            rectangle,
            FormatterFormat::Plain,
            true,
            true,
        )
    }

    pub fn read_ansi_screen(
        &self,
        start: (u16, u32),
        end: (u16, u32),
        rectangle: bool,
        unwrap: bool,
    ) -> Result<String, Error> {
        self.read_formatted_selection(
            ghostty_screen_point(start.0, start.1),
            ghostty_screen_point(end.0, end.1),
            rectangle,
            FormatterFormat::Vt,
            unwrap,
            true,
        )
    }

    pub fn keyboard_state_ansi(&self) -> Result<String, Error> {
        self.format_keyboard_state_ansi(false)
    }

    fn format_keyboard_state_ansi(&self, kitty_keyboard: bool) -> Result<String, Error> {
        let mut formatter: ffi::GhosttyFormatter = ptr::null_mut();
        let options = ffi::GhosttyFormatterTerminalOptions {
            size: mem::size_of::<ffi::GhosttyFormatterTerminalOptions>(),
            emit: FormatterFormat::Vt.as_raw(),
            unwrap: false,
            trim: false,
            extra: ffi::GhosttyFormatterTerminalExtra {
                size: mem::size_of::<ffi::GhosttyFormatterTerminalExtra>(),
                keyboard: true,
                screen: ffi::GhosttyFormatterScreenExtra {
                    size: mem::size_of::<ffi::GhosttyFormatterScreenExtra>(),
                    kitty_keyboard,
                    ..Default::default()
                },
                ..Default::default()
            },
            selection: ptr::null(),
        };
        unsafe {
            ffi::ghostty_formatter_terminal_new(ptr::null(), &mut formatter, self.raw, options)
                .into_result()?;
        }

        let mut out_ptr = ptr::null_mut();
        let mut out_len = 0usize;
        let result = unsafe {
            ffi::ghostty_formatter_format_alloc(formatter, ptr::null(), &mut out_ptr, &mut out_len)
        };
        unsafe {
            ffi::ghostty_formatter_free(formatter);
        }
        result.into_result()?;

        let text = if out_len == 0 {
            String::new()
        } else {
            let bytes = unsafe { slice::from_raw_parts(out_ptr.cast_const(), out_len) };
            String::from_utf8_lossy(bytes).into_owned()
        };

        if !out_ptr.is_null() {
            unsafe {
                ffi::ghostty_free(ptr::null(), out_ptr, out_len);
            }
        }

        Ok(text)
    }

    fn read_formatted_selection(
        &self,
        start: ffi::GhosttyPoint,
        end: ffi::GhosttyPoint,
        rectangle: bool,
        format: FormatterFormat,
        unwrap: bool,
        trim: bool,
    ) -> Result<String, Error> {
        let mut start_ref = ffi::GhosttyGridRef {
            size: mem::size_of::<ffi::GhosttyGridRef>(),
            ..Default::default()
        };
        let mut end_ref = ffi::GhosttyGridRef {
            size: mem::size_of::<ffi::GhosttyGridRef>(),
            ..Default::default()
        };
        unsafe {
            ffi::ghostty_terminal_grid_ref(self.raw, start, &mut start_ref).into_result()?;
            ffi::ghostty_terminal_grid_ref(self.raw, end, &mut end_ref).into_result()?;
        }

        let selection = ffi::GhosttySelection {
            size: mem::size_of::<ffi::GhosttySelection>(),
            start: start_ref,
            end: end_ref,
            rectangle,
        };
        let mut formatter: ffi::GhosttyFormatter = ptr::null_mut();
        let options = ffi::GhosttyFormatterTerminalOptions {
            size: mem::size_of::<ffi::GhosttyFormatterTerminalOptions>(),
            emit: format.as_raw(),
            unwrap,
            trim,
            extra: ffi::GhosttyFormatterTerminalExtra {
                size: mem::size_of::<ffi::GhosttyFormatterTerminalExtra>(),
                screen: ffi::GhosttyFormatterScreenExtra {
                    size: mem::size_of::<ffi::GhosttyFormatterScreenExtra>(),
                    ..Default::default()
                },
                ..Default::default()
            },
            selection: &selection,
        };
        unsafe {
            ffi::ghostty_formatter_terminal_new(ptr::null(), &mut formatter, self.raw, options)
                .into_result()?;
        }

        let mut out_ptr = ptr::null_mut();
        let mut out_len = 0usize;
        let result = unsafe {
            ffi::ghostty_formatter_format_alloc(formatter, ptr::null(), &mut out_ptr, &mut out_len)
        };
        unsafe {
            ffi::ghostty_formatter_free(formatter);
        }
        result.into_result()?;

        let text = if out_len == 0 {
            String::new()
        } else {
            let bytes = unsafe { slice::from_raw_parts(out_ptr.cast_const(), out_len) };
            String::from_utf8_lossy(bytes).into_owned()
        };

        if !out_ptr.is_null() {
            unsafe {
                ffi::ghostty_free(ptr::null(), out_ptr, out_len);
            }
        }

        Ok(text)
    }

    pub fn scroll_viewport_delta(&mut self, delta: isize) {
        let viewport = ffi::GhosttyTerminalScrollViewport {
            tag: ffi::GhosttyTerminalScrollViewportTag_GHOSTTY_SCROLL_VIEWPORT_DELTA,
            value: ffi::GhosttyTerminalScrollViewportValue { delta },
        };
        // SAFETY: self.raw is valid and viewport value matches the tag.
        unsafe {
            ffi::ghostty_terminal_scroll_viewport(self.raw, viewport);
        }
    }

    pub fn cols(&self) -> Result<u16, Error> {
        self.get_u16(ffi::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_COLS)
    }

    pub fn rows(&self) -> Result<u16, Error> {
        self.get_u16(ffi::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_ROWS)
    }

    fn get_u16(&self, data: ffi::GhosttyTerminalData) -> Result<u16, Error> {
        let mut out = 0u16;
        // SAFETY: out points to a u16 matching the requested terminal data type.
        unsafe {
            ffi::ghostty_terminal_get(self.raw, data, (&mut out as *mut u16).cast())
                .into_result()?;
        }
        Ok(out)
    }

    fn get_usize(&self, data: ffi::GhosttyTerminalData) -> Result<usize, Error> {
        let mut out = 0usize;
        unsafe {
            ffi::ghostty_terminal_get(self.raw, data, (&mut out as *mut usize).cast())
                .into_result()?;
        }
        Ok(out)
    }

    fn get_bool(&self, data: ffi::GhosttyTerminalData) -> Result<bool, Error> {
        let mut out = false;
        unsafe {
            ffi::ghostty_terminal_get(self.raw, data, (&mut out as *mut bool).cast())
                .into_result()?;
        }
        Ok(out)
    }

    fn raw(&self) -> ffi::GhosttyTerminal {
        self.raw
    }
}

// SAFETY: these opaque handles are only used behind external synchronization in pane runtime.
unsafe impl Send for Terminal {}

impl Drop for Terminal {
    fn drop(&mut self) {
        // SAFETY: freeing null or live handles is allowed by the C API. The gesture
        // must release its tracked refs before the terminal that owns them is freed.
        unsafe {
            ffi::ghostty_selection_gesture_free(self.selection_gesture, self.raw);
            #[cfg(windows)]
            ffi::ghostty_tracked_grid_ref_free(self.tracked_row);
            ffi::ghostty_terminal_free(self.raw);
        }
    }
}

struct SelectionGestureEvent {
    raw: ffi::GhosttySelectionGestureEvent,
}

impl SelectionGestureEvent {
    fn new(event_type: ffi::GhosttySelectionGestureEventType) -> Result<Self, Error> {
        let mut raw = ptr::null_mut();
        unsafe {
            ffi::ghostty_selection_gesture_event_new(ptr::null(), &mut raw, event_type)
                .into_result()?;
        }
        Ok(Self { raw })
    }

    fn set<T>(
        &self,
        option: ffi::GhosttySelectionGestureEventOption,
        value: &T,
    ) -> Result<(), Error> {
        unsafe {
            ffi::ghostty_selection_gesture_event_set(self.raw, option, (value as *const T).cast())
                .into_result()
        }
    }
}

impl Drop for SelectionGestureEvent {
    fn drop(&mut self) {
        unsafe { ffi::ghostty_selection_gesture_event_free(self.raw) };
    }
}

// ^ Crossterm reports terminal cells, while Ghostty's gesture thresholds use pixels.
// A fixed virtual cell preserves repeat-click and drag semantics without guessed font metrics.
const SELECTION_CELL_SIZE: u32 = 10;

fn selection_position(x: u16, y: u16) -> ffi::GhosttySurfacePosition {
    ffi::GhosttySurfacePosition {
        x: f64::from(u32::from(x) * SELECTION_CELL_SIZE + SELECTION_CELL_SIZE / 2),
        y: f64::from(u32::from(y) * SELECTION_CELL_SIZE + SELECTION_CELL_SIZE / 2),
    }
}

fn same_grid_ref(left: &ffi::GhosttyGridRef, right: &ffi::GhosttyGridRef) -> bool {
    left.node == right.node && left.x == right.x && left.y == right.y
}

fn ghostty_viewport_point(x: u16, y: u32) -> ffi::GhosttyPoint {
    ffi::GhosttyPoint {
        tag: ffi::GhosttyPointTag_GHOSTTY_POINT_TAG_VIEWPORT,
        value: ffi::GhosttyPointValue {
            coordinate: ffi::GhosttyPointCoordinate { x, y },
        },
    }
}

fn ghostty_screen_point(x: u16, y: u32) -> ffi::GhosttyPoint {
    ffi::GhosttyPoint {
        tag: ffi::GhosttyPointTag_GHOSTTY_POINT_TAG_SCREEN,
        value: ffi::GhosttyPointValue {
            coordinate: ffi::GhosttyPointCoordinate { x, y },
        },
    }
}

pub struct RenderState {
    raw: ffi::GhosttyRenderState,
}

impl RenderState {
    pub fn new() -> Result<Self, Error> {
        let mut raw = ptr::null_mut();
        // SAFETY: valid out pointer and null allocator use default allocator.
        unsafe {
            ffi::ghostty_render_state_new(ptr::null(), &mut raw).into_result()?;
        }
        Ok(Self { raw })
    }

    pub fn update(&mut self, terminal: &Terminal) -> Result<(), Error> {
        // SAFETY: both handles are valid for the duration of the call.
        unsafe { ffi::ghostty_render_state_update(self.raw, terminal.raw()).into_result() }
    }

    pub fn cols(&self) -> Result<u16, Error> {
        self.get_u16(ffi::GhosttyRenderStateData_GHOSTTY_RENDER_STATE_DATA_COLS)
    }

    pub fn rows(&self) -> Result<u16, Error> {
        self.get_u16(ffi::GhosttyRenderStateData_GHOSTTY_RENDER_STATE_DATA_ROWS)
    }

    pub fn dirty(&self) -> Result<Dirty, Error> {
        let mut out = ffi::GhosttyRenderStateDirty_GHOSTTY_RENDER_STATE_DIRTY_FALSE;
        // SAFETY: out points to the matching enum storage for the requested data kind.
        unsafe {
            ffi::ghostty_render_state_get(
                self.raw,
                ffi::GhosttyRenderStateData_GHOSTTY_RENDER_STATE_DATA_DIRTY,
                (&mut out as *mut ffi::GhosttyRenderStateDirty).cast(),
            )
            .into_result()?;
        }
        Ok(Dirty::from_raw(out))
    }

    pub fn cursor_visible(&self) -> Result<bool, Error> {
        self.get_bool(ffi::GhosttyRenderStateData_GHOSTTY_RENDER_STATE_DATA_CURSOR_VISIBLE)
    }

    pub fn cursor_blinking(&self) -> Result<bool, Error> {
        self.get_bool(ffi::GhosttyRenderStateData_GHOSTTY_RENDER_STATE_DATA_CURSOR_BLINKING)
    }

    pub fn cursor_visual_style(&self) -> Result<CursorVisualStyle, Error> {
        let mut out: ffi::GhosttyRenderStateCursorVisualStyle = 0;
        // SAFETY: out points to the matching enum storage for the requested data kind.
        unsafe {
            ffi::ghostty_render_state_get(
                self.raw,
                ffi::GhosttyRenderStateData_GHOSTTY_RENDER_STATE_DATA_CURSOR_VISUAL_STYLE,
                (&mut out as *mut ffi::GhosttyRenderStateCursorVisualStyle).cast(),
            )
            .into_result()?;
        }
        Ok(CursorVisualStyle::from_raw(out))
    }

    pub fn cursor_viewport(&self) -> Result<Option<CursorViewport>, Error> {
        if !self.get_bool(
            ffi::GhosttyRenderStateData_GHOSTTY_RENDER_STATE_DATA_CURSOR_VIEWPORT_HAS_VALUE,
        )? {
            return Ok(None);
        }
        Ok(Some(CursorViewport {
            x: self
                .get_u16(ffi::GhosttyRenderStateData_GHOSTTY_RENDER_STATE_DATA_CURSOR_VIEWPORT_X)?,
            y: self
                .get_u16(ffi::GhosttyRenderStateData_GHOSTTY_RENDER_STATE_DATA_CURSOR_VIEWPORT_Y)?,
            wide_tail: self.get_bool(
                ffi::GhosttyRenderStateData_GHOSTTY_RENDER_STATE_DATA_CURSOR_VIEWPORT_WIDE_TAIL,
            )?,
        }))
    }

    pub fn colors(&self) -> Result<RenderColors, Error> {
        let mut colors = ffi::GhosttyRenderStateColors {
            size: mem::size_of::<ffi::GhosttyRenderStateColors>(),
            ..Default::default()
        };
        unsafe {
            ffi::ghostty_render_state_colors_get(self.raw, &mut colors).into_result()?;
        }
        Ok(RenderColors {
            background: colors.background.into(),
            foreground: colors.foreground.into(),
            palette: colors.palette.map(Into::into),
        })
    }

    pub fn set_dirty(&mut self, dirty: Dirty) -> Result<(), Error> {
        let value = dirty.as_raw();
        // SAFETY: value pointer matches the expected option type.
        unsafe {
            ffi::ghostty_render_state_set(
                self.raw,
                ffi::GhosttyRenderStateOption_GHOSTTY_RENDER_STATE_OPTION_DIRTY,
                (&value as *const ffi::GhosttyRenderStateDirty).cast(),
            )
            .into_result()
        }
    }

    pub fn populate_row_iterator<'a>(
        &'a self,
        iterator: &'a mut RowIterator,
    ) -> Result<RowIter<'a>, Error> {
        // SAFETY: iterator raw handle is valid and will not outlive self.
        unsafe {
            ffi::ghostty_render_state_get(
                self.raw,
                ffi::GhosttyRenderStateData_GHOSTTY_RENDER_STATE_DATA_ROW_ITERATOR,
                (&mut iterator.raw as *mut ffi::GhosttyRenderStateRowIterator).cast(),
            )
            .into_result()?;
        }
        Ok(RowIter {
            iterator,
            _state: PhantomData,
        })
    }

    fn get_u16(&self, data: ffi::GhosttyRenderStateData) -> Result<u16, Error> {
        let mut out = 0u16;
        // SAFETY: out points to a u16 matching the requested render-state data type.
        unsafe {
            ffi::ghostty_render_state_get(self.raw, data, (&mut out as *mut u16).cast())
                .into_result()?;
        }
        Ok(out)
    }

    fn get_bool(&self, data: ffi::GhosttyRenderStateData) -> Result<bool, Error> {
        let mut out = false;
        unsafe {
            ffi::ghostty_render_state_get(self.raw, data, (&mut out as *mut bool).cast())
                .into_result()?;
        }
        Ok(out)
    }
}

// SAFETY: these opaque handles are only used behind external synchronization in pane runtime.
unsafe impl Send for RenderState {}

impl Drop for RenderState {
    fn drop(&mut self) {
        // SAFETY: freeing a null or live handle is allowed by the C API.
        unsafe {
            ffi::ghostty_render_state_free(self.raw);
        }
    }
}

pub struct KeyEvent {
    raw: ffi::GhosttyKeyEvent,
}

impl KeyEvent {
    pub fn new() -> Result<Self, Error> {
        let mut raw = ptr::null_mut();
        unsafe { ffi::ghostty_key_event_new(ptr::null(), &mut raw).into_result()? };
        Ok(Self { raw })
    }

    pub fn set_action(&mut self, action: ffi::GhosttyKeyAction) {
        unsafe { ffi::ghostty_key_event_set_action(self.raw, action) }
    }

    pub fn set_key(&mut self, key: u32) {
        unsafe { ffi::ghostty_key_event_set_key(self.raw, key) }
    }

    pub fn set_mods(&mut self, mods: u16) {
        unsafe { ffi::ghostty_key_event_set_mods(self.raw, mods) }
    }

    pub fn set_utf8(&mut self, text: &str) {
        unsafe {
            ffi::ghostty_key_event_set_utf8(self.raw, text.as_ptr().cast::<c_char>(), text.len())
        }
    }

    pub fn set_unshifted_codepoint(&mut self, codepoint: u32) {
        unsafe { ffi::ghostty_key_event_set_unshifted_codepoint(self.raw, codepoint) }
    }
}

impl Drop for KeyEvent {
    fn drop(&mut self) {
        unsafe { ffi::ghostty_key_event_free(self.raw) }
    }
}

pub struct KeyEncoder {
    raw: ffi::GhosttyKeyEncoder,
}

impl KeyEncoder {
    pub fn new() -> Result<Self, Error> {
        let mut raw = ptr::null_mut();
        unsafe { ffi::ghostty_key_encoder_new(ptr::null(), &mut raw).into_result()? };
        Ok(Self { raw })
    }

    pub fn set_from_terminal(&mut self, terminal: &Terminal) {
        unsafe { ffi::ghostty_key_encoder_setopt_from_terminal(self.raw, terminal.raw()) }
    }

    pub fn encode(&mut self, event: &KeyEvent) -> Result<Vec<u8>, Error> {
        encode_with_retry(|buf, len, out_len| unsafe {
            ffi::ghostty_key_encoder_encode(self.raw, event.raw, buf, len, out_len)
        })
    }
}

// SAFETY: the opaque encoder handle is only used behind external synchronization in pane runtime.
unsafe impl Send for KeyEncoder {}

impl Drop for KeyEncoder {
    fn drop(&mut self) {
        unsafe { ffi::ghostty_key_encoder_free(self.raw) }
    }
}

pub struct MouseEvent {
    raw: ffi::GhosttyMouseEvent,
}

impl MouseEvent {
    pub fn new() -> Result<Self, Error> {
        let mut raw = ptr::null_mut();
        unsafe { ffi::ghostty_mouse_event_new(ptr::null(), &mut raw).into_result()? };
        Ok(Self { raw })
    }

    pub fn set_action(&mut self, action: ffi::GhosttyMouseAction) {
        unsafe { ffi::ghostty_mouse_event_set_action(self.raw, action) }
    }

    pub fn set_button(&mut self, button: ffi::GhosttyMouseButton) {
        unsafe { ffi::ghostty_mouse_event_set_button(self.raw, button) }
    }

    pub fn clear_button(&mut self) {
        unsafe { ffi::ghostty_mouse_event_clear_button(self.raw) }
    }

    pub fn set_mods(&mut self, mods: u16) {
        unsafe { ffi::ghostty_mouse_event_set_mods(self.raw, mods) }
    }

    pub fn set_position(&mut self, x: f32, y: f32) {
        unsafe {
            ffi::ghostty_mouse_event_set_position(self.raw, ffi::GhosttyMousePosition { x, y })
        }
    }
}

impl Drop for MouseEvent {
    fn drop(&mut self) {
        unsafe { ffi::ghostty_mouse_event_free(self.raw) }
    }
}

pub struct MouseEncoder {
    raw: ffi::GhosttyMouseEncoder,
}

// ^ wsx keeps this handle behind the terminal runtime mutex and never invokes
// the C encoder concurrently.
unsafe impl Send for MouseEncoder {}

impl MouseEncoder {
    pub fn new() -> Result<Self, Error> {
        let mut raw = ptr::null_mut();
        unsafe { ffi::ghostty_mouse_encoder_new(ptr::null(), &mut raw).into_result()? };
        Ok(Self { raw })
    }

    pub fn set_from_terminal(&mut self, terminal: &Terminal) {
        unsafe { ffi::ghostty_mouse_encoder_setopt_from_terminal(self.raw, terminal.raw()) }
    }

    pub fn set_size(
        &mut self,
        screen_width: u32,
        screen_height: u32,
        cell_width: u32,
        cell_height: u32,
    ) {
        let size = ffi::GhosttyMouseEncoderSize {
            size: std::mem::size_of::<ffi::GhosttyMouseEncoderSize>(),
            screen_width,
            screen_height,
            cell_width,
            cell_height,
            padding_top: 0,
            padding_bottom: 0,
            padding_right: 0,
            padding_left: 0,
        };
        unsafe {
            ffi::ghostty_mouse_encoder_setopt(
                self.raw,
                ffi::GhosttyMouseEncoderOption_GHOSTTY_MOUSE_ENCODER_OPT_SIZE,
                (&size as *const ffi::GhosttyMouseEncoderSize).cast(),
            )
        }
    }

    pub fn encode(&mut self, event: &MouseEvent) -> Result<Vec<u8>, Error> {
        encode_with_retry(|buf, len, out_len| unsafe {
            ffi::ghostty_mouse_encoder_encode(self.raw, event.raw, buf, len, out_len)
        })
    }
}

impl Drop for MouseEncoder {
    fn drop(&mut self) {
        unsafe { ffi::ghostty_mouse_encoder_free(self.raw) }
    }
}

fn encode_with_retry(
    mut encode: impl FnMut(*mut c_char, usize, *mut usize) -> ffi::GhosttyResult,
) -> Result<Vec<u8>, Error> {
    let mut required = 0usize;
    let result = encode(ptr::null_mut(), 0, &mut required);
    if result != ffi::GhosttyResult_GHOSTTY_OUT_OF_SPACE {
        result.into_result()?;
    }
    let mut buffer = vec![0u8; required.max(16)];
    let mut written = 0usize;
    encode(
        buffer.as_mut_ptr().cast::<c_char>(),
        buffer.len(),
        &mut written,
    )
    .into_result()?;
    buffer.truncate(written);
    Ok(buffer)
}

pub struct RowIterator {
    raw: ffi::GhosttyRenderStateRowIterator,
}

impl RowIterator {
    pub fn new() -> Result<Self, Error> {
        let mut raw = ptr::null_mut();
        // SAFETY: valid out pointer and null allocator use default allocator.
        unsafe {
            ffi::ghostty_render_state_row_iterator_new(ptr::null(), &mut raw).into_result()?;
        }
        Ok(Self { raw })
    }
}

// SAFETY: these opaque handles are only used behind external synchronization in pane runtime.
unsafe impl Send for RowIterator {}

impl Drop for RowIterator {
    fn drop(&mut self) {
        // SAFETY: freeing a null or live handle is allowed by the C API.
        unsafe {
            ffi::ghostty_render_state_row_iterator_free(self.raw);
        }
    }
}

pub struct RowIter<'a> {
    iterator: &'a mut RowIterator,
    _state: PhantomData<&'a RenderState>,
}

impl<'a> RowIter<'a> {
    pub fn next(&mut self) -> bool {
        // SAFETY: iterator handle is valid while self is alive.
        unsafe { ffi::ghostty_render_state_row_iterator_next(self.iterator.raw) }
    }

    pub fn dirty(&self) -> Result<bool, Error> {
        let mut dirty = false;
        // SAFETY: dirty output matches requested row data type.
        unsafe {
            ffi::ghostty_render_state_row_get(
                self.iterator.raw,
                ffi::GhosttyRenderStateRowData_GHOSTTY_RENDER_STATE_ROW_DATA_DIRTY,
                (&mut dirty as *mut bool).cast(),
            )
            .into_result()?;
        }
        Ok(dirty)
    }

    #[cfg(windows)]
    pub fn wrap_state(&self) -> Result<(bool, bool), Error> {
        let mut row = 0;
        // SAFETY: row output matches requested row data type.
        unsafe {
            ffi::ghostty_render_state_row_get(
                self.iterator.raw,
                ffi::GhosttyRenderStateRowData_GHOSTTY_RENDER_STATE_ROW_DATA_RAW,
                (&mut row as *mut ffi::GhosttyRow).cast(),
            )
            .into_result()?;
        }
        let mut soft_wrapped = false;
        // SAFETY: wrap output matches requested row data type.
        unsafe {
            ffi::ghostty_row_get(
                row,
                ffi::GhosttyRowData_GHOSTTY_ROW_DATA_WRAP,
                (&mut soft_wrapped as *mut bool).cast(),
            )
            .into_result()?;
        }
        let mut wrap_continuation = false;
        // SAFETY: wrap continuation output matches requested row data type.
        unsafe {
            ffi::ghostty_row_get(
                row,
                ffi::GhosttyRowData_GHOSTTY_ROW_DATA_WRAP_CONTINUATION,
                (&mut wrap_continuation as *mut bool).cast(),
            )
            .into_result()?;
        }
        Ok((soft_wrapped, wrap_continuation))
    }

    pub fn clear_dirty(&mut self) -> Result<(), Error> {
        self.set_dirty(false)
    }

    pub fn set_dirty(&mut self, dirty: bool) -> Result<(), Error> {
        // SAFETY: dirty pointer matches the expected row option type.
        unsafe {
            ffi::ghostty_render_state_row_set(
                self.iterator.raw,
                ffi::GhosttyRenderStateRowOption_GHOSTTY_RENDER_STATE_ROW_OPTION_DIRTY,
                (&dirty as *const bool).cast(),
            )
            .into_result()
        }
    }

    pub fn selection(&self) -> Result<Option<RowSelection>, Error> {
        let mut selection = ffi::GhosttyRenderStateRowSelection {
            size: mem::size_of::<ffi::GhosttyRenderStateRowSelection>(),
            ..Default::default()
        };
        let result = unsafe {
            ffi::ghostty_render_state_row_get(
                self.iterator.raw,
                ffi::GhosttyRenderStateRowData_GHOSTTY_RENDER_STATE_ROW_DATA_SELECTION,
                (&mut selection as *mut ffi::GhosttyRenderStateRowSelection).cast(),
            )
        };
        match result {
            ffi::GhosttyResult_GHOSTTY_SUCCESS => Ok(Some(RowSelection {
                start_x: selection.start_x,
                end_x: selection.end_x,
            })),
            ffi::GhosttyResult_GHOSTTY_NO_VALUE => Ok(None),
            other => Err(Error(other)),
        }
    }

    pub fn populate_cells<'b>(
        &'b mut self,
        cells: &'b mut RowCells,
    ) -> Result<RowCellIter<'b>, Error> {
        // SAFETY: cells raw handle is valid and will not outlive the current row borrow.
        unsafe {
            ffi::ghostty_render_state_row_get(
                self.iterator.raw,
                ffi::GhosttyRenderStateRowData_GHOSTTY_RENDER_STATE_ROW_DATA_CELLS,
                (&mut cells.raw as *mut ffi::GhosttyRenderStateRowCells).cast(),
            )
            .into_result()?;
        }
        Ok(RowCellIter { cells })
    }
}

pub struct RowCells {
    raw: ffi::GhosttyRenderStateRowCells,
}

impl RowCells {
    pub fn new() -> Result<Self, Error> {
        let mut raw = ptr::null_mut();
        // SAFETY: valid out pointer and null allocator use default allocator.
        unsafe {
            ffi::ghostty_render_state_row_cells_new(ptr::null(), &mut raw).into_result()?;
        }
        Ok(Self { raw })
    }
}

// SAFETY: these opaque handles are only used behind external synchronization in pane runtime.
unsafe impl Send for RowCells {}

impl Drop for RowCells {
    fn drop(&mut self) {
        // SAFETY: freeing a null or live handle is allowed by the C API.
        unsafe {
            ffi::ghostty_render_state_row_cells_free(self.raw);
        }
    }
}

pub struct RowCellIter<'a> {
    cells: &'a mut RowCells,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellBasicData {
    pub wide: CellWide,
    pub has_hyperlink: bool,
    pub has_styling: bool,
    pub style: CellStyle,
}

impl Default for CellBasicData {
    fn default() -> Self {
        Self {
            wide: CellWide::Narrow,
            has_hyperlink: false,
            has_styling: false,
            style: CellStyle::default(),
        }
    }
}

impl<'a> RowCellIter<'a> {
    pub fn next(&mut self) -> bool {
        // SAFETY: cells handle is valid while self is alive.
        unsafe { ffi::ghostty_render_state_row_cells_next(self.cells.raw) }
    }

    fn raw_cell(&self) -> Result<ffi::GhosttyCell, Error> {
        let mut raw = ffi::GhosttyCell::default();
        unsafe {
            ffi::ghostty_render_state_row_cells_get(
                self.cells.raw,
                ffi::GhosttyRenderStateRowCellsData_GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_RAW,
                (&mut raw as *mut ffi::GhosttyCell).cast(),
            )
            .into_result()?;
        }
        Ok(raw)
    }

    pub fn basic_data(&self) -> Result<CellBasicData, Error> {
        let mut raw = ffi::GhosttyCell::default();
        let mut style = ffi::GhosttyStyle {
            size: mem::size_of::<ffi::GhosttyStyle>(),
            ..Default::default()
        };
        let mut has_styling = false;
        let row_keys = [
            ffi::GhosttyRenderStateRowCellsData_GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_RAW,
            ffi::GhosttyRenderStateRowCellsData_GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_STYLE,
            ffi::GhosttyRenderStateRowCellsData_GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_HAS_STYLING,
        ];
        let mut row_values = [
            (&mut raw as *mut ffi::GhosttyCell).cast::<c_void>(),
            (&mut style as *mut ffi::GhosttyStyle).cast::<c_void>(),
            (&mut has_styling as *mut bool).cast::<c_void>(),
        ];
        let mut written = 0usize;
        unsafe {
            ffi::ghostty_render_state_row_cells_get_multi(
                self.cells.raw,
                row_keys.len(),
                row_keys.as_ptr(),
                row_values.as_mut_ptr(),
                &mut written,
            )
            .into_result()?;
        }

        let mut wide = ffi::GhosttyCellWide_GHOSTTY_CELL_WIDE_NARROW;
        let mut has_hyperlink = false;
        let cell_keys = [
            ffi::GhosttyCellData_GHOSTTY_CELL_DATA_WIDE,
            ffi::GhosttyCellData_GHOSTTY_CELL_DATA_HAS_HYPERLINK,
        ];
        let mut cell_values = [
            (&mut wide as *mut ffi::GhosttyCellWide).cast::<c_void>(),
            (&mut has_hyperlink as *mut bool).cast::<c_void>(),
        ];
        unsafe {
            ffi::ghostty_cell_get_multi(
                raw,
                cell_keys.len(),
                cell_keys.as_ptr(),
                cell_values.as_mut_ptr(),
                &mut written,
            )
            .into_result()?;
        }

        Ok(CellBasicData {
            wide: CellWide::from_raw(wide),
            has_hyperlink,
            has_styling,
            style: style.into(),
        })
    }

    pub fn fg_color(&self) -> Result<Option<RgbColor>, Error> {
        let mut color = ffi::GhosttyColorRgb::default();
        let result = unsafe {
            ffi::ghostty_render_state_row_cells_get(
                self.cells.raw,
                ffi::GhosttyRenderStateRowCellsData_GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_FG_COLOR,
                (&mut color as *mut ffi::GhosttyColorRgb).cast(),
            )
        };
        match result {
            ffi::GhosttyResult_GHOSTTY_SUCCESS => Ok(Some(color.into())),
            ffi::GhosttyResult_GHOSTTY_INVALID_VALUE => Ok(None),
            other => Err(Error(other)),
        }
    }

    pub fn bg_color(&self) -> Result<Option<RgbColor>, Error> {
        let mut color = ffi::GhosttyColorRgb::default();
        let result = unsafe {
            ffi::ghostty_render_state_row_cells_get(
                self.cells.raw,
                ffi::GhosttyRenderStateRowCellsData_GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_BG_COLOR,
                (&mut color as *mut ffi::GhosttyColorRgb).cast(),
            )
        };
        match result {
            ffi::GhosttyResult_GHOSTTY_SUCCESS => Ok(Some(color.into())),
            ffi::GhosttyResult_GHOSTTY_INVALID_VALUE => Ok(None),
            other => Err(Error(other)),
        }
    }

    fn raw_cell_text_into(&self, text: &mut String) -> Result<(), Error> {
        let raw = self.raw_cell()?;
        let mut has_text = false;
        unsafe {
            ffi::ghostty_cell_get(
                raw,
                ffi::GhosttyCellData_GHOSTTY_CELL_DATA_HAS_TEXT,
                (&mut has_text as *mut bool).cast(),
            )
            .into_result()?;
        }
        if !has_text {
            return Ok(());
        }

        let mut codepoint = 0u32;
        unsafe {
            ffi::ghostty_cell_get(
                raw,
                ffi::GhosttyCellData_GHOSTTY_CELL_DATA_CODEPOINT,
                (&mut codepoint as *mut u32).cast(),
            )
            .into_result()?;
        }
        if let Some(ch) = char::from_u32(codepoint) {
            text.push(ch);
        }
        Ok(())
    }

    pub fn grapheme_text_into(&self, bytes: &mut Vec<u8>, text: &mut String) -> Result<(), Error> {
        text.clear();
        bytes.clear();

        let mut buffer = ffi::GhosttyBuffer {
            ptr: ptr::null_mut(),
            cap: 0,
            len: 0,
        };
        let result = unsafe {
            ffi::ghostty_render_state_row_cells_get(
                self.cells.raw,
                ffi::GhosttyRenderStateRowCellsData_GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_UTF8,
                (&mut buffer as *mut ffi::GhosttyBuffer).cast(),
            )
        };
        match result {
            ffi::GhosttyResult_GHOSTTY_SUCCESS if buffer.len == 0 => {
                return self.raw_cell_text_into(text);
            }
            ffi::GhosttyResult_GHOSTTY_SUCCESS => {
                return Err(Error(ffi::GhosttyResult_GHOSTTY_INVALID_VALUE));
            }
            ffi::GhosttyResult_GHOSTTY_OUT_OF_SPACE => {}
            other => return Err(Error(other)),
        }

        if buffer.len == 0 {
            return self.raw_cell_text_into(text);
        }
        bytes.resize(buffer.len, 0);
        let mut buffer = ffi::GhosttyBuffer {
            ptr: bytes.as_mut_ptr(),
            cap: bytes.len(),
            len: 0,
        };
        unsafe {
            ffi::ghostty_render_state_row_cells_get(
                self.cells.raw,
                ffi::GhosttyRenderStateRowCellsData_GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_UTF8,
                (&mut buffer as *mut ffi::GhosttyBuffer).cast(),
            )
            .into_result()?;
        }
        if buffer.len > bytes.len() {
            return Err(Error(ffi::GhosttyResult_GHOSTTY_OUT_OF_SPACE));
        }
        bytes.truncate(buffer.len);
        match std::str::from_utf8(bytes) {
            Ok(value) => text.push_str(value),
            Err(_) => text.push_str(&String::from_utf8_lossy(bytes)),
        }
        Ok(())
    }
}
