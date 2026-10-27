//! What a client sees: frames, patches, raw replay and the bounded agent observation.
// ^ docs/terminal-raw-stream.md, docs/terminal-synchronized-output.md
use super::*;

pub(super) const AGENT_OBSERVATION_ROWS: usize = 12;

pub(super) const AGENT_OBSERVATION_TEXT_BYTES: usize = 32 * 1024;

pub(super) const AGENT_OBSERVATION_TITLE_BYTES: usize = 1024;

pub(super) const MAX_RAW_OUTPUT_BYTES: usize = 256 * 1024;

// Modes a replay restores in a client emulator, with the value after its reset.
pub(super) const RAW_REPLAY_MODES: &[(u16, bool)] = &[
    (ghostty::MODE_APPLICATION_CURSOR_KEYS, false),
    (7, true),  // wraparound
    (25, true), // cursor visible
    (9, false), // mouse tracking: X10, normal, button, any
    (1000, false),
    (1002, false),
    (1003, false),
    (ghostty::MODE_FOCUS_EVENT, false),
    (ghostty::MODE_MOUSE_UTF8, false),
    (ghostty::MODE_MOUSE_SGR, false),
    (ghostty::MODE_MOUSE_SGR_PIXELS, false),
    (ghostty::MODE_BRACKETED_PASTE, false),
];

/// A screen a client emulator can start from, and the tap position it was taken at.
pub struct RawReplay {
    pub ansi: Vec<u8>,
    pub rows: u16,
    pub cols: u16,
    pub sequence: u64,
    pub truncated: bool,
}

pub struct RawSample {
    pub rows: u16,
    pub cols: u16,
    /// `None`: the tap no longer holds the bytes after the requested position.
    pub output: Option<Vec<u8>>,
    pub clipboard_writes: Vec<Vec<u8>>,
    /// A synchronized update is open and within its bound (docs/terminal-synchronized-output.md).
    pub synchronized_output: bool,
}

pub struct PresentationSample {
    pub revision: u64,
    pub synchronized_output: bool,
    pub update: Result<Option<TerminalUpdate>, TerminalError>,
    pub clipboard_writes: Vec<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentTerminalObservation {
    pub revision: u64,
    pub title: String,
    pub text: String,
}

pub(super) fn cell_width(width: ghostty::CellWide) -> wsx_core::runtime::CellWidth {
    match width {
        ghostty::CellWide::Narrow => wsx_core::runtime::CellWidth::Narrow,
        ghostty::CellWide::Wide => wsx_core::runtime::CellWidth::Wide,
        ghostty::CellWide::SpacerHead => wsx_core::runtime::CellWidth::SpacerHead,
        ghostty::CellWide::SpacerTail => wsx_core::runtime::CellWidth::SpacerTail,
    }
}

pub(super) fn normalize_cell_symbol(symbol: &mut String, width: wsx_core::runtime::CellWidth) {
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

pub(super) fn blank(fg: ghostty::RgbColor) -> Cell {
    Cell {
        symbol: String::new(),
        fg: Some([fg.r, fg.g, fg.b]),
        bg: None,
        modifiers: CellModifiers::default(),
        width: wsx_core::runtime::CellWidth::Narrow,
    }
}

pub(super) fn truncate_utf8_start(value: &mut String, max_bytes: usize) {
    if value.len() <= max_bytes {
        return;
    }
    let mut start = value.len() - max_bytes;
    while !value.is_char_boundary(start) {
        start += 1;
    }
    value.drain(..start);
}

pub(super) fn truncate_utf8_end(value: &mut String, max_bytes: usize) {
    if value.len() <= max_bytes {
        return;
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
}

impl TerminalRuntime {
    pub fn agent_observation(&self) -> Result<AgentTerminalObservation, TerminalError> {
        if let Some(error) = lock(&self.shared.error).clone() {
            return Err(TerminalError::Runtime(error));
        }
        let emulator = lock(&self.shared.emulator);
        let terminal = &emulator.terminal;
        let cols = terminal.cols()?;
        let total_rows = terminal.total_rows()?;
        let visible_rows = usize::from(terminal.rows()?);
        // ^ The live bottom is where the program's output ends, not the last screen row:
        // a program that has written little, such as a fresh agent that shows a dialog,
        // leaves the rows below it empty. Read the screen, then keep the last written rows.
        let rows = visible_rows.min(total_rows);
        let mut text = if rows == 0 || cols == 0 {
            String::new()
        } else {
            let start = total_rows - rows;
            terminal.read_text_screen(
                (0, u32::try_from(start).unwrap_or(u32::MAX)),
                (
                    cols.saturating_sub(1),
                    u32::try_from(total_rows - 1).unwrap_or(u32::MAX),
                ),
                false,
            )?
        };
        text.truncate(text.trim_end().len());
        if let Some((start, _)) = text.rmatch_indices('\n').nth(AGENT_OBSERVATION_ROWS - 1) {
            text.drain(..=start);
        }
        truncate_utf8_start(&mut text, AGENT_OBSERVATION_TEXT_BYTES);
        let mut title = terminal.title()?;
        truncate_utf8_end(&mut title, AGENT_OBSERVATION_TITLE_BYTES);
        Ok(AgentTerminalObservation {
            revision: self.shared.revision.load(Ordering::Acquire),
            title,
            text,
        })
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
            selection: Vec::new(),
        })
    }

    #[cfg(test)]
    pub(super) fn frame_update(
        &self,
        base_revision: Option<u64>,
    ) -> Result<TerminalUpdate, TerminalError> {
        if let Some(error) = lock(&self.shared.error).clone() {
            return Err(TerminalError::Runtime(error));
        }
        let mut emulator = lock(&self.shared.emulator);
        let revision = self.shared.revision.load(Ordering::Acquire);
        self.frame_update_locked(&mut emulator, revision, base_revision)
    }

    pub fn presentation_sample(
        &self,
        base_revision: Option<u64>,
        emit_frame: bool,
    ) -> PresentationSample {
        let runtime_error = lock(&self.shared.error).clone();
        let mut emulator = lock(&self.shared.emulator);
        let revision = self.shared.revision.load(Ordering::Acquire);
        let synchronized_output = emulator.synchronized_output(Instant::now());
        let clipboard_writes = emulator.terminal.take_clipboard_writes();
        let update = if let Some(error) = runtime_error {
            Err(TerminalError::Runtime(error))
        } else if base_revision.is_none()
            || (emit_frame && !synchronized_output && base_revision != Some(revision))
        {
            self.frame_update_locked(&mut emulator, revision, base_revision)
                .map(Some)
        } else {
            Ok(None)
        };
        PresentationSample {
            revision,
            synchronized_output,
            update,
            clipboard_writes,
        }
    }

    pub(super) fn frame_update_locked(
        &self,
        emulator: &mut Emulator,
        revision: u64,
        base_revision: Option<u64>,
    ) -> Result<TerminalUpdate, TerminalError> {
        let Emulator {
            terminal, render, ..
        } = emulator;
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
        let mut selection = Vec::new();
        let mut bytes = Vec::new();
        let mut symbol = String::new();
        for row in 0..rows {
            if !rows_iter.next() {
                if full {
                    all_cells.extend((0..cols).map(|_| blank(colors.foreground)));
                }
                continue;
            }
            if let Some(range) = rows_iter.selection()? {
                selection.push(TerminalSelectionRange {
                    row,
                    start_col: range.start_x,
                    end_col: range.end_x,
                });
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
                selection,
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
                selection,
            })
        }
    }

    // ^ docs/terminal-raw-stream.md: the replay and its tap position come from one
    // emulator lock, so the replay plus every later byte has no gap and no repeat.
    pub fn raw_replay(&self) -> Result<RawReplay, TerminalError> {
        let mut emulator = lock(&self.shared.emulator);
        let Emulator {
            terminal,
            render,
            raw,
            ..
        } = &mut *emulator;
        render.update(terminal)?;
        let cols = terminal.cols()?;
        let rows = terminal.rows()?;
        let total_rows = terminal.total_rows()?.max(usize::from(rows));
        let scrollback_rows = total_rows - usize::from(rows);
        let end_col = cols.saturating_sub(1);
        let row = |index: usize| u32::try_from(index).unwrap_or(u32::MAX);
        let mut ansi = String::new();
        let mut truncated = false;
        if terminal.active_screen()? == ghostty::ActiveScreen::Alternate {
            ansi.push_str("\x1b[?1049h");
        } else if scrollback_rows > 0 {
            let (history, history_truncated) =
                bounded_handoff_ansi(terminal, cols, scrollback_rows, MAX_HANDOFF_ANSI_BYTES)?;
            truncated = history_truncated;
            // Scroll exactly the written history lines off the screen, so the
            // screen below starts at row 1 as it does in this emulator.
            let lines = history.lines().count().min(usize::from(rows));
            if lines > 0 {
                ansi.push_str(&history);
                ansi.push_str(&format!("\x1b[0m\x1b[{rows};1H"));
                ansi.push_str(&"\n".repeat(lines));
            }
        }
        ansi.push_str("\x1b[H");
        ansi.push_str(&terminal.read_ansi_screen(
            (0, row(scrollback_rows)),
            (end_col, row(total_rows - 1)),
            false,
            false,
        )?);
        ansi.push_str("\x1b[0m");
        for (mode, after_reset) in RAW_REPLAY_MODES {
            // A mode this emulator does not know stays at the client's reset value.
            let value = terminal.mode_get(*mode).unwrap_or(*after_reset);
            if value != *after_reset {
                ansi.push_str(&format!("\x1b[?{mode}{}", if value { 'h' } else { 'l' }));
            }
        }
        let (cursor_x, cursor_y) = render
            .cursor_viewport()?
            .map_or((0, 0), |cursor| (cursor.x, cursor.y));
        ansi.push_str(&format!("\x1b[{};{}H", cursor_y + 1, cursor_x + 1));
        raw.ring.get_or_insert_with(VecDeque::new);
        Ok(RawReplay {
            ansi: ansi.into_bytes(),
            rows,
            cols,
            sequence: raw.end,
            truncated,
        })
    }

    pub fn raw_sample(&self, after: u64) -> Result<RawSample, TerminalError> {
        let mut emulator = lock(&self.shared.emulator);
        Ok(RawSample {
            rows: emulator.terminal.rows()?,
            cols: emulator.terminal.cols()?,
            output: emulator.raw.read(after, MAX_RAW_OUTPUT_BYTES),
            clipboard_writes: emulator.terminal.take_clipboard_writes(),
            synchronized_output: emulator.synchronized_output(Instant::now()),
        })
    }
}
