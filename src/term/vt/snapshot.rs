use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::Arc;

use ghostty_sys::*;

use super::{
    Cell, Cursor, CursorShape, MAX_HOLD, Rgb, SYNC_OUTPUT, Snapshot, Vt, blank_cell, reported_path,
    terminal_text,
};

/// Whether the render-state row the iterator is on soft-wraps into the row
/// below, so a URL can be read across the break.
unsafe fn row_wraps(rows: GhosttyRenderStateRowIterator) -> bool {
    unsafe {
        let mut raw: GhosttyRow = 0;
        if ghostty_render_state_row_get(
            rows,
            GHOSTTY_RENDER_STATE_ROW_DATA_RAW,
            &mut raw as *mut _ as *mut c_void,
        ) != GHOSTTY_SUCCESS
        {
            return false;
        }
        let mut wrap = false;
        ghostty_row_get(
            raw,
            GHOSTTY_ROW_DATA_WRAP,
            &mut wrap as *mut _ as *mut c_void,
        ) == GHOSTTY_SUCCESS
            && wrap
    }
}

/// `GHOSTTY_STYLE_FLAG_*` and the underline field are macros. Bindgen skips them.
const STYLE_BOLD: GhosttyStyleFlags = 1 << 0;
const STYLE_ITALIC: GhosttyStyleFlags = 1 << 1;
const STYLE_FAINT: GhosttyStyleFlags = 1 << 2;
const STYLE_INVERSE: GhosttyStyleFlags = 1 << 4;
const STYLE_INVISIBLE: GhosttyStyleFlags = 1 << 5;
const STYLE_STRIKE: GhosttyStyleFlags = 1 << 6;
const STYLE_UNDERLINE_MASK: GhosttyStyleFlags = 0x7 << 8;

impl Vt {
    pub fn snapshot(&mut self, command: &str, cwd: &str, exited: bool) -> Snapshot {
        unsafe {
            // Read these first: the strings are borrowed from the terminal and
            // copied before the render-state calls below.
            let title = terminal_text(self.term, GHOSTTY_TERMINAL_DATA_TITLE);
            let pwd = terminal_text(self.term, GHOSTTY_TERMINAL_DATA_PWD);
            // Match endpoints must be read before anything below touches the
            // terminal; they are snapshots valid only for this frame.
            let search = self.search_view();
            if let Some(since) = (*self.shared.as_ptr()).held_since
                && since.elapsed() >= MAX_HOLD
            {
                let cfg = GhosttyTerminalModeConfig {
                    mode: SYNC_OUTPUT,
                    value: false,
                };
                ghostty_terminal_set(
                    self.term,
                    GHOSTTY_TERMINAL_OPT_MODE,
                    &cfg as *const _ as *const c_void,
                );
                (*self.shared.as_ptr()).held_since = None;
            }
            let state = (*self.shared.as_ptr()).state;
            if (*self.shared.as_ptr()).held_since.is_none() {
                ghostty_render_state_update(state, self.term);
            }
            let mut colors = GhosttyRenderStateColors {
                size: std::mem::size_of::<GhosttyRenderStateColors>(),
                ..Default::default()
            };
            ghostty_render_state_get(
                state,
                GHOSTTY_RENDER_STATE_DATA_COLORS,
                &mut colors as *mut _ as *mut c_void,
            );
            let fg_default = Rgb(
                colors.foreground.r,
                colors.foreground.g,
                colors.foreground.b,
            );
            let bg = Rgb(
                colors.background.r,
                colors.background.g,
                colors.background.b,
            );

            // Clean rows keep their Arc (see Ghostty "Row Identity"). A resize
            // or default-color change does not always mark rows dirty, so those
            // force a full reread.
            let with = (self.cols, bg, fg_default);
            let mut dirty = GHOSTTY_RENDER_STATE_DIRTY_FULL;
            ghostty_render_state_get(
                state,
                GHOSTTY_RENDER_STATE_DATA_DIRTY,
                &mut dirty as *mut _ as *mut c_void,
            );
            let all = dirty == GHOSTTY_RENDER_STATE_DIRTY_FULL || with != self.seen_with;
            let mut seen = HashMap::with_capacity(self.rows as usize);
            let mut rows = Vec::with_capacity(self.rows as usize);
            let mut wraps = Vec::with_capacity(self.rows as usize);
            ghostty_render_state_get(
                state,
                GHOSTTY_RENDER_STATE_DATA_ROW_ITERATOR,
                &mut self.rows_it as *mut _ as *mut c_void,
            );
            while ghostty_render_state_row_iterator_next(self.rows_it) {
                let mut sel = GhosttyRenderStateRowSelection {
                    size: std::mem::size_of::<GhosttyRenderStateRowSelection>(),
                    ..Default::default()
                };
                let selected = ghostty_render_state_row_get(
                    self.rows_it,
                    GHOSTTY_RENDER_STATE_ROW_DATA_SELECTION,
                    &mut sel as *mut _ as *mut c_void,
                ) == GHOSTTY_SUCCESS;
                let selection = selected.then_some((sel.start_x, sel.end_x));
                let mut id = GhosttyRenderStateRowId::default();
                let known = ghostty_render_state_row_get(
                    self.rows_it,
                    GHOSTTY_RENDER_STATE_ROW_DATA_ID,
                    &mut id as *mut _ as *mut c_void,
                ) == GHOSTTY_SUCCESS;
                let mut row_dirty = true;
                if !all {
                    ghostty_render_state_row_get(
                        self.rows_it,
                        GHOSTTY_RENDER_STATE_ROW_DATA_DIRTY,
                        &mut row_dirty as *mut _ as *mut c_void,
                    );
                }
                let reuse = (known && !row_dirty)
                    .then(|| self.seen.get(&id.bits))
                    .flatten()
                    .filter(|(_, prev)| *prev == selection)
                    .map(|(row, _)| Arc::clone(row));
                let row = match reuse {
                    Some(row) => row,
                    None => self.read_row_cells(fg_default, bg, selection),
                };
                if known {
                    seen.insert(id.bits, (Arc::clone(&row), selection));
                }
                rows.push(row);
                wraps.push(row_wraps(self.rows_it));
            }
            (self.seen, self.seen_with) = (seen, with);

            let mut cur = GhosttyRenderStateCursor {
                size: std::mem::size_of::<GhosttyRenderStateCursor>(),
                ..Default::default()
            };
            ghostty_render_state_get(
                state,
                GHOSTTY_RENDER_STATE_DATA_CURSOR,
                &mut cur as *mut _ as *mut c_void,
            );
            let cell_x = if cur.wide_tail {
                cur.viewport_x.saturating_sub(1)
            } else {
                cur.viewport_x
            };
            let cursor_cell = cur.viewport_has_value.then_some((cell_x, cur.viewport_y));
            let cursor = (cur.visible && cur.viewport_has_value).then_some(Cursor {
                x: cell_x,
                y: cur.viewport_y,
                shape: match cur.visual_style {
                    GHOSTTY_CURSOR_STYLE_BAR => CursorShape::Bar,
                    GHOSTTY_CURSOR_STYLE_UNDERLINE => CursorShape::Underline,
                    GHOSTTY_CURSOR_STYLE_BLOCK_HOLLOW => CursorShape::Hollow,
                    _ => CursorShape::Block,
                },
                color: if colors.cursor_has_value {
                    Rgb(colors.cursor.r, colors.cursor.g, colors.cursor.b)
                } else {
                    fg_default
                },
            });
            ghostty_render_state_clean(state);
            Snapshot {
                rows: self.rows,
                bg,
                fg: fg_default,
                cells: rows,
                wrap: wraps,
                cursor,
                cursor_cell,
                command: command.to_string(),
                cwd: reported_path(&pwd).unwrap_or_else(|| cwd.to_string()),
                title,
                exited,
                spawn_error: None,
                mouse_tracking: self.mouse_tracking(),
                search,
            }
        }
    }

    /// Reads the cells of the row the render-state iterator is on.
    unsafe fn read_row_cells(
        &mut self,
        fg_default: Rgb,
        bg_default: Rgb,
        selection: Option<(u16, u16)>,
    ) -> Arc<Vec<Cell>> {
        unsafe {
            ghostty_render_state_row_get(
                self.rows_it,
                GHOSTTY_RENDER_STATE_ROW_DATA_CELLS,
                &mut self.cells_it as *mut _ as *mut c_void,
            );
            let mut row = Vec::with_capacity(self.cols as usize);
            let mut x: u16 = 0;
            while ghostty_render_state_row_cells_next(self.cells_it) {
                let in_sel = selection.is_some_and(|(start, end)| x >= start && x <= end);
                row.push(self.read_cell(fg_default, bg_default, in_sel));
                x = x.saturating_add(1);
            }
            row.resize(self.cols as usize, blank_cell(fg_default));
            Arc::new(row)
        }
    }

    unsafe fn read_cell(&mut self, fg_default: Rgb, bg_default: Rgb, selected: bool) -> Cell {
        unsafe {
            let cells = self.cells_it;
            let mut raw: GhosttyCell = Default::default();
            ghostty_render_state_row_cells_get(
                cells,
                GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_RAW,
                &mut raw as *mut _ as *mut c_void,
            );
            let mut wide: GhosttyCellWide = GHOSTTY_CELL_WIDE_NARROW;
            ghostty_cell_get(
                raw,
                GHOSTTY_CELL_DATA_WIDE,
                &mut wide as *mut _ as *mut c_void,
            );
            let mut style = GhosttyStyle::default();
            ghostty_render_state_row_cells_get(
                cells,
                GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_STYLE,
                &mut style as *mut _ as *mut c_void,
            );
            let mut fg = GhosttyColorRgb::default();
            let fg = if ghostty_render_state_row_cells_get(
                cells,
                GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_FG_COLOR,
                &mut fg as *mut _ as *mut c_void,
            ) == GHOSTTY_SUCCESS
            {
                Rgb(fg.r, fg.g, fg.b)
            } else {
                fg_default
            };
            let mut bg = GhosttyColorRgb::default();
            let bg = (ghostty_render_state_row_cells_get(
                cells,
                GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_BG_COLOR,
                &mut bg as *mut _ as *mut c_void,
            ) == GHOSTTY_SUCCESS)
                .then_some(Rgb(bg.r, bg.g, bg.b));
            let (mut fg, mut bg) = (fg, bg);
            if (style.flags & STYLE_INVERSE) != 0 {
                let inverted = bg.unwrap_or(bg_default);
                bg = Some(fg);
                fg = inverted;
            }
            if selected {
                bg = Some(self.colors.selection);
            }
            let mut text = String::new();
            let mut hyperlink = false;
            ghostty_cell_get(
                raw,
                GHOSTTY_CELL_DATA_HAS_HYPERLINK,
                &mut hyperlink as *mut _ as *mut c_void,
            );
            if wide != GHOSTTY_CELL_WIDE_SPACER_TAIL
                && wide != GHOSTTY_CELL_WIDE_SPACER_HEAD
                && (style.flags & STYLE_INVISIBLE) == 0
            {
                let mut len: u32 = 0;
                ghostty_render_state_row_cells_get(
                    cells,
                    GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_LEN,
                    &mut len as *mut _ as *mut c_void,
                );
                if len > 0 {
                    let mut stack = [0u32; 8];
                    let mut heap = Vec::new();
                    let buf: &mut [u32] = if (len as usize) <= stack.len() {
                        &mut stack[..len as usize]
                    } else {
                        heap.resize(len as usize, 0);
                        &mut heap
                    };
                    let mut graphemes = GhosttyCodepointBuffer {
                        ptr: buf.as_mut_ptr(),
                        cap: buf.len(),
                        len: 0,
                    };
                    ghostty_render_state_row_cells_get(
                        cells,
                        GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_BUF,
                        &mut graphemes as *mut _ as *mut c_void,
                    );
                    let written = graphemes.len.min(buf.len());
                    text = buf[..written]
                        .iter()
                        .copied()
                        .filter_map(char::from_u32)
                        .collect();
                }
            }
            Cell {
                text,
                fg,
                bg,
                bold: (style.flags & STYLE_BOLD) != 0,
                italic: (style.flags & STYLE_ITALIC) != 0,
                faint: (style.flags & STYLE_FAINT) != 0,
                underline: (style.flags & STYLE_UNDERLINE_MASK) != 0,
                strike: (style.flags & STYLE_STRIKE) != 0,
                wide: wide == GHOSTTY_CELL_WIDE_WIDE,
                hyperlink,
            }
        }
    }

    /// The visible screen as plain text, preceded by up to `history` rows of
    /// scrollback.
    ///
    /// Formatted directly from Ghostty's grid without disturbing the render
    /// state, synchronized-output hold, row cache, or viewport scroll position.
    pub fn content_text(&mut self, history: usize) -> String {
        let want_history = if self.alt_screen() { 0 } else { history };
        let Some(bar) = self.scrollbar() else {
            return String::new();
        };
        let total = bar.total as usize;
        let view = (bar.len as usize).max(usize::from(self.rows));
        if total == 0 || self.cols == 0 {
            return String::new();
        }
        let bottom = total.saturating_sub(view);
        let start_row = bottom.saturating_sub(want_history);
        let end_row = total - 1;
        unsafe {
            let mut start_ref = GhosttyGridRef::default();
            let mut start_pt = GhosttyPoint {
                tag: GHOSTTY_POINT_TAG_SCREEN,
                ..Default::default()
            };
            start_pt.value.coordinate = GhosttyPointCoordinate {
                x: 0,
                y: start_row.min(u32::MAX as usize) as u32,
            };
            let mut end_ref = GhosttyGridRef::default();
            let mut end_pt = GhosttyPoint {
                tag: GHOSTTY_POINT_TAG_SCREEN,
                ..Default::default()
            };
            end_pt.value.coordinate = GhosttyPointCoordinate {
                x: self.cols.saturating_sub(1),
                y: end_row.min(u32::MAX as usize) as u32,
            };
            if ghostty_terminal_grid_ref(self.term, start_pt, &mut start_ref) != GHOSTTY_SUCCESS
                || ghostty_terminal_grid_ref(self.term, end_pt, &mut end_ref) != GHOSTTY_SUCCESS
            {
                return String::new();
            }
            let mut formatter = std::ptr::null_mut();
            if ghostty_formatter_terminal_new(std::ptr::null(), &mut formatter, self.term)
                != GHOSTTY_SUCCESS
            {
                return String::new();
            }
            let emit = GHOSTTY_FORMATTER_FORMAT_PLAIN;
            let unwrap = false;
            let trim = true;
            let selection = GhosttySelection {
                start: start_ref,
                end: end_ref,
                rectangle: false,
            };
            ghostty_formatter_set(
                formatter,
                GHOSTTY_FORMATTER_OPT_EMIT,
                &emit as *const _ as *const c_void,
            );
            ghostty_formatter_set(
                formatter,
                GHOSTTY_FORMATTER_OPT_UNWRAP,
                &unwrap as *const _ as *const c_void,
            );
            ghostty_formatter_set(
                formatter,
                GHOSTTY_FORMATTER_OPT_TRIM,
                &trim as *const _ as *const c_void,
            );
            ghostty_formatter_set(
                formatter,
                GHOSTTY_FORMATTER_OPT_SELECTION,
                &selection as *const _ as *const c_void,
            );
            let mut ptr: *mut u8 = std::ptr::null_mut();
            let mut len = 0usize;
            let status =
                ghostty_formatter_format_alloc(formatter, std::ptr::null(), &mut ptr, &mut len);
            let text = if status == GHOSTTY_SUCCESS && !ptr.is_null() && len > 0 {
                let text =
                    String::from_utf8_lossy(std::slice::from_raw_parts(ptr, len)).into_owned();
                ghostty_free(std::ptr::null(), ptr, len);
                text
            } else {
                String::new()
            };
            ghostty_formatter_free(formatter);
            let mut lines: Vec<&str> = text.lines().collect();
            while lines.last().is_some_and(|l| l.is_empty()) {
                lines.pop();
            }
            lines.join("\n")
        }
    }
}
