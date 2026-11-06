//! libghostty-vt on one thread: bytes in, a cell grid out, query replies back.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ghostty_sys::*;

const SYNC_OUTPUT: GhosttyMode = 2026;
const GRAPHEME_CLUSTER: GhosttyMode = 2027;
const MAX_HOLD: Duration = Duration::from_millis(1000);
const SCROLLBACK_MAX_LINES: usize = 10_000;
const SCROLLBACK_MAX_BYTES: usize = 10_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

/// Default fg/bg/cursor and the 16 ANSI colors Ghostty paints with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TermColors {
    pub fg: Rgb,
    pub bg: Rgb,
    pub cursor: Rgb,
    pub palette: [Rgb; 16],
}

impl Default for TermColors {
    fn default() -> Self {
        Self {
            fg: Rgb(0xdd, 0xdd, 0xdd),
            bg: Rgb(0x1e, 0x1e, 0x1e),
            cursor: Rgb(0xf5, 0xf5, 0xf5),
            palette: [
                Rgb(0x1e, 0x1e, 0x1e),
                Rgb(0xe0, 0x4b, 0x4b),
                Rgb(0x6a, 0xc4, 0x6a),
                Rgb(0xe0, 0xc4, 0x4b),
                Rgb(0x4b, 0x8a, 0xe0),
                Rgb(0xc4, 0x6a, 0xc4),
                Rgb(0x4b, 0xc4, 0xc4),
                Rgb(0xdd, 0xdd, 0xdd),
                Rgb(0x80, 0x80, 0x80),
                Rgb(0xff, 0x6b, 0x6b),
                Rgb(0x8a, 0xe0, 0x8a),
                Rgb(0xff, 0xe0, 0x6b),
                Rgb(0x6b, 0xaa, 0xff),
                Rgb(0xe0, 0x8a, 0xe0),
                Rgb(0x6b, 0xe0, 0xe0),
                Rgb(0xff, 0xff, 0xff),
            ],
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Cell {
    /// Empty for a blank cell and for the second column of a wide character.
    pub text: String,
    pub fg: Rgb,
    pub bg: Option<Rgb>,
    pub bold: bool,
    pub italic: bool,
    pub faint: bool,
    pub underline: bool,
    pub strike: bool,
    pub wide: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorShape {
    Block,
    Bar,
    Underline,
    Hollow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cursor {
    pub x: u16,
    pub y: u16,
    pub shape: CursorShape,
    pub color: Rgb,
}

/// One published screen. Rows that libghostty kept clean reuse the same [`Arc`].
#[derive(Clone, Debug)]
pub struct Snapshot {
    /// Grid width in cells; kept for resize/IME callers even when a frame only
    /// paints `cells`.
    #[allow(dead_code)]
    pub cols: u16,
    pub rows: u16,
    pub bg: Rgb,
    pub cells: Vec<Arc<Vec<Cell>>>,
    /// Every row changed (scroll, resize, new colors): none of the arcs is kept.
    pub all: bool,
    pub cursor: Option<Cursor>,
    /// Cursor cell even when the program hides it — where IME puts its preview.
    pub cursor_cell: Option<(u16, u16)>,
    pub command: String,
    pub cwd: String,
    pub exited: bool,
    pub spawn_error: Option<String>,
}

impl Snapshot {
    pub fn blank(command: &str, cwd: &str) -> Self {
        Self::blank_with(command, cwd, TermColors::default().bg)
    }

    pub fn blank_with(command: &str, cwd: &str, bg: Rgb) -> Self {
        Self {
            cols: 80,
            rows: 24,
            bg,
            cells: Vec::new(),
            all: true,
            cursor: None,
            cursor_cell: None,
            command: command.to_string(),
            cwd: cwd.to_string(),
            exited: false,
            spawn_error: None,
        }
    }

    #[cfg(test)]
    pub fn plain_text(&self) -> String {
        self.cells
            .iter()
            .map(|row| {
                let mut text = String::new();
                for cell in row.iter() {
                    text.push_str(&cell.text);
                }
                text.trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn blank_cell(fg: Rgb) -> Cell {
    Cell {
        text: String::new(),
        fg,
        bg: None,
        bold: false,
        italic: false,
        faint: false,
        underline: false,
        strike: false,
        wide: false,
    }
}

/// What the UI asks the session thread to encode. The encoder has to see the
/// terminal's current keyboard mode, so this stays data until that thread.
#[derive(Clone, Debug)]
pub struct KeyInput {
    pub key: String,
    pub key_char: Option<String>,
    pub shift: bool,
    pub control: bool,
    pub alt: bool,
}

struct Shared {
    state: GhosttyRenderState,
    size: GhosttySizeReportSize,
    held_since: Option<Instant>,
    replies: Vec<u8>,
}

fn ok(status: GhosttyResult, what: &str) {
    assert!(status == GHOSTTY_SUCCESS, "ghostty {what} failed: {status}");
}

unsafe extern "C" fn on_write_pty(
    _term: GhosttyTerminal,
    userdata: *mut c_void,
    data: *const u8,
    len: usize,
) {
    let shared = unsafe { &mut *(userdata as *mut Shared) };
    if !data.is_null() && len > 0 {
        shared
            .replies
            .extend_from_slice(unsafe { std::slice::from_raw_parts(data, len) });
    }
}

unsafe extern "C" fn on_render_hold(term: GhosttyTerminal, userdata: *mut c_void, held: bool) {
    let shared = unsafe { &mut *(userdata as *mut Shared) };
    if held {
        unsafe { ghostty_render_state_update(shared.state, term) };
        shared.held_since = Some(Instant::now());
    } else {
        shared.held_since = None;
    }
}

unsafe extern "C" fn on_size(
    _term: GhosttyTerminal,
    userdata: *mut c_void,
    out: *mut GhosttySizeReportSize,
) -> bool {
    let shared = unsafe { &*(userdata as *const Shared) };
    if shared.size.cell_width == 0 || shared.size.cell_height == 0 {
        return false;
    }
    unsafe { *out = shared.size };
    true
}

pub struct Vt {
    term: GhosttyTerminal,
    shared: Box<Shared>,
    rows_it: GhosttyRenderStateRowIterator,
    cells_it: GhosttyRenderStateRowCells,
    keys: GhosttyKeyEncoder,
    key_event: GhosttyKeyEvent,
    cols: u16,
    rows: u16,
    colors: TermColors,
    /// Rows read last frame by libghostty's row id. A clean id reuses the Arc.
    seen: HashMap<[u64; 2], Arc<Vec<Cell>>>,
    /// Width and default colors those rows were read with.
    seen_with: (u16, Rgb, Rgb),
}

impl Vt {
    #[cfg(test)]
    pub fn new(cols: u16, rows: u16) -> Self {
        Self::with_colors(cols, rows, TermColors::default())
    }

    pub fn with_colors(cols: u16, rows: u16, colors: TermColors) -> Self {
        unsafe {
            let cols = cols.max(1);
            let rows = rows.max(1);
            let mut term = std::ptr::null_mut();
            ok(
                ghostty_terminal_new(std::ptr::null(), &mut term, cols, rows),
                "terminal_new",
            );
            let cluster = GhosttyTerminalModeConfig {
                mode: GRAPHEME_CLUSTER,
                value: true,
            };
            ghostty_terminal_set(
                term,
                GHOSTTY_TERMINAL_OPT_MODE_DEFAULT,
                &cluster as *const _ as *const c_void,
            );
            let mut state = std::ptr::null_mut();
            ok(
                ghostty_render_state_new(std::ptr::null(), &mut state),
                "render_state_new",
            );
            let mut rows_it = std::ptr::null_mut();
            ok(
                ghostty_render_state_row_iterator_new(std::ptr::null(), &mut rows_it),
                "row_iterator_new",
            );
            let mut cells_it = std::ptr::null_mut();
            ok(
                ghostty_render_state_row_cells_new(std::ptr::null(), &mut cells_it),
                "row_cells_new",
            );
            let mut keys = std::ptr::null_mut();
            ok(
                ghostty_key_encoder_new(std::ptr::null(), &mut keys),
                "key_encoder_new",
            );
            let mut key_event = std::ptr::null_mut();
            ok(
                ghostty_key_event_new(std::ptr::null(), &mut key_event),
                "key_event_new",
            );

            let mut shared = Box::new(Shared {
                state,
                size: GhosttySizeReportSize {
                    rows,
                    columns: cols,
                    cell_width: 8,
                    cell_height: 16,
                },
                held_since: None,
                replies: Vec::new(),
            });
            let userdata = &mut *shared as *mut Shared as *const c_void;
            ghostty_terminal_set(term, GHOSTTY_TERMINAL_OPT_USERDATA, userdata);
            let hold: GhosttyTerminalRenderHoldFn = Some(on_render_hold);
            ghostty_terminal_set(
                term,
                GHOSTTY_TERMINAL_OPT_RENDER_HOLD,
                std::mem::transmute::<GhosttyTerminalRenderHoldFn, *const c_void>(hold),
            );
            let size: GhosttyTerminalSizeFn = Some(on_size);
            ghostty_terminal_set(
                term,
                GHOSTTY_TERMINAL_OPT_SIZE,
                std::mem::transmute::<GhosttyTerminalSizeFn, *const c_void>(size),
            );
            let write: GhosttyTerminalWritePtyFn = Some(on_write_pty);
            ghostty_terminal_set(
                term,
                GHOSTTY_TERMINAL_OPT_WRITE_PTY,
                std::mem::transmute::<GhosttyTerminalWritePtyFn, *const c_void>(write),
            );
            ok(
                ghostty_terminal_set(
                    term,
                    GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_BYTES,
                    &SCROLLBACK_MAX_BYTES as *const _ as *const c_void,
                ),
                "scrollback_max_bytes",
            );
            ok(
                ghostty_terminal_set(
                    term,
                    GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_LINES,
                    &SCROLLBACK_MAX_LINES as *const _ as *const c_void,
                ),
                "scrollback_max_lines",
            );

            let mut vt = Self {
                term,
                shared,
                rows_it,
                cells_it,
                keys,
                key_event,
                cols,
                rows,
                colors,
                seen: HashMap::new(),
                seen_with: (0, Rgb(0, 0, 0), Rgb(0, 0, 0)),
            };
            vt.apply_colors();
            vt
        }
    }

    pub fn set_colors(&mut self, colors: TermColors) {
        if self.colors == colors {
            return;
        }
        self.colors = colors;
        self.seen.clear();
        self.apply_colors();
    }

    fn apply_colors(&mut self) {
        let c = |rgb: Rgb| GhosttyColorRgb {
            r: rgb.0,
            g: rgb.1,
            b: rgb.2,
        };
        let fg = c(self.colors.fg);
        let bg = c(self.colors.bg);
        let cursor = c(self.colors.cursor);
        let palette = self.colors.palette.map(c);
        unsafe {
            ghostty_terminal_set(
                self.term,
                GHOSTTY_TERMINAL_OPT_COLOR_FOREGROUND,
                &fg as *const _ as *const c_void,
            );
            ghostty_terminal_set(
                self.term,
                GHOSTTY_TERMINAL_OPT_COLOR_BACKGROUND,
                &bg as *const _ as *const c_void,
            );
            ghostty_terminal_set(
                self.term,
                GHOSTTY_TERMINAL_OPT_COLOR_CURSOR,
                &cursor as *const _ as *const c_void,
            );
            ghostty_terminal_set(
                self.term,
                GHOSTTY_TERMINAL_OPT_COLOR_PALETTE,
                &palette as *const _ as *const c_void,
            );
        }
    }

    pub fn write(&mut self, bytes: &[u8]) {
        unsafe { ghostty_terminal_vt_write(self.term, bytes.as_ptr(), bytes.len()) };
    }

    pub fn take_replies(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.shared.replies)
    }

    /// How long to wait before a held synchronized-output frame must be shown.
    pub fn hold_wait(&self) -> Option<Duration> {
        self.shared
            .held_since
            .map(|since| MAX_HOLD.saturating_sub(since.elapsed()))
    }

    pub fn resize(&mut self, cols: u16, rows: u16, cell_w: u32, cell_h: u32) {
        if cols == 0 || rows == 0 {
            return;
        }
        self.cols = cols;
        self.rows = rows;
        self.seen.clear();
        self.shared.size = GhosttySizeReportSize {
            rows,
            columns: cols,
            cell_width: cell_w.max(1),
            cell_height: cell_h.max(1),
        };
        unsafe { ghostty_terminal_resize(self.term, cols, rows, cell_w.max(1), cell_h.max(1)) };
    }

    pub fn snapshot(&mut self, command: &str, cwd: &str, exited: bool) -> Snapshot {
        unsafe {
            if let Some(since) = self.shared.held_since
                && since.elapsed() > MAX_HOLD
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
                self.shared.held_since = None;
            }
            let state = self.shared.state;
            if self.shared.held_since.is_none() {
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
            ghostty_render_state_get(
                state,
                GHOSTTY_RENDER_STATE_DATA_ROW_ITERATOR,
                &mut self.rows_it as *mut _ as *mut c_void,
            );
            while ghostty_render_state_row_iterator_next(self.rows_it) {
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
                let row = if known && !row_dirty {
                    self.seen.get(&id.bits).cloned()
                } else {
                    None
                };
                let row = match row {
                    Some(row) => row,
                    None => {
                        ghostty_render_state_row_get(
                            self.rows_it,
                            GHOSTTY_RENDER_STATE_ROW_DATA_CELLS,
                            &mut self.cells_it as *mut _ as *mut c_void,
                        );
                        let mut row = Vec::with_capacity(self.cols as usize);
                        while ghostty_render_state_row_cells_next(self.cells_it) {
                            row.push(self.read_cell(fg_default));
                        }
                        row.resize(self.cols as usize, blank_cell(fg_default));
                        Arc::new(row)
                    }
                };
                if known {
                    seen.insert(id.bits, Arc::clone(&row));
                }
                rows.push(row);
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
                    GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BAR => CursorShape::Bar,
                    GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_UNDERLINE => CursorShape::Underline,
                    GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BLOCK_HOLLOW => CursorShape::Hollow,
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
                cols: self.cols,
                rows: self.rows,
                bg,
                cells: rows,
                all,
                cursor,
                cursor_cell,
                command: command.to_string(),
                cwd: cwd.to_string(),
                exited,
                spawn_error: None,
            }
        }
    }

    unsafe fn read_cell(&mut self, fg_default: Rgb) -> Cell {
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
            let mut style = GhosttyStyle {
                size: std::mem::size_of::<GhosttyStyle>(),
                ..Default::default()
            };
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
            if style.inverse {
                let inverted = bg.unwrap_or(self.colors.bg);
                bg = Some(fg);
                fg = inverted;
            }
            let mut text = String::new();
            if wide != GHOSTTY_CELL_WIDE_SPACER_TAIL
                && wide != GHOSTTY_CELL_WIDE_SPACER_HEAD
                && !style.invisible
            {
                let mut len: u32 = 0;
                ghostty_render_state_row_cells_get(
                    cells,
                    GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_LEN,
                    &mut len as *mut _ as *mut c_void,
                );
                if len > 0 {
                    let mut cps = vec![0u32; len as usize];
                    ghostty_render_state_row_cells_get(
                        cells,
                        GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_BUF,
                        cps.as_mut_ptr() as *mut c_void,
                    );
                    text = cps.into_iter().filter_map(char::from_u32).collect();
                }
            }
            Cell {
                text,
                fg,
                bg,
                bold: style.bold,
                italic: style.italic,
                faint: style.faint,
                underline: style.underline != 0,
                strike: style.strikethrough,
                wide: wide == GHOSTTY_CELL_WIDE_WIDE,
            }
        }
    }

    pub fn encode_input(&mut self, input: &KeyInput) -> Vec<u8> {
        let Some((key, shifted, unshifted)) = map_key(&input.key) else {
            return Vec::new();
        };
        let mut mods: GhosttyMods = 0;
        if input.shift || shifted {
            mods |= GHOSTTY_MODS_SHIFT as GhosttyMods;
        }
        if input.control {
            mods |= GHOSTTY_MODS_CTRL as GhosttyMods;
        }
        if input.alt {
            mods |= GHOSTTY_MODS_ALT as GhosttyMods;
        }
        let text = if input.control {
            None
        } else if input.key == "space" {
            Some(" ".to_string())
        } else {
            input.key_char.clone()
        };
        let printable = text
            .as_deref()
            .is_some_and(|text| !text.is_empty() && !text.chars().any(|c| c.is_control()));
        let consumed = if printable {
            mods & GHOSTTY_MODS_SHIFT as GhosttyMods
        } else {
            0
        };
        self.encode_key(
            key,
            mods,
            consumed,
            text.filter(|_| printable).as_deref(),
            unshifted,
        )
    }

    fn encode_key(
        &mut self,
        key: GhosttyKey,
        mods: GhosttyMods,
        consumed: GhosttyMods,
        text: Option<&str>,
        unshifted: Option<char>,
    ) -> Vec<u8> {
        unsafe {
            ghostty_key_encoder_setopt_from_terminal(self.keys, self.term);
            let event = self.key_event;
            ghostty_key_event_set_action(event, GHOSTTY_KEY_ACTION_PRESS);
            ghostty_key_event_set_key(event, key);
            ghostty_key_event_set_mods(event, mods);
            ghostty_key_event_set_consumed_mods(event, consumed);
            match text {
                Some(text) => ghostty_key_event_set_utf8(event, text.as_ptr().cast(), text.len()),
                None => ghostty_key_event_set_utf8(event, std::ptr::null(), 0),
            }
            ghostty_key_event_set_unshifted_codepoint(event, unshifted.map_or(0, |c| c as u32));
            let mut buf = [0u8; 128];
            let mut len = 0usize;
            let status = ghostty_key_encoder_encode(
                self.keys,
                event,
                buf.as_mut_ptr() as *mut _,
                buf.len(),
                &mut len,
            );
            if status != GHOSTTY_SUCCESS {
                return Vec::new();
            }
            buf[..len].to_vec()
        }
    }
}

fn map_key(key: &str) -> Option<(GhosttyKey, bool, Option<char>)> {
    let named = match key {
        "enter" => GHOSTTY_KEY_ENTER,
        "tab" => GHOSTTY_KEY_TAB,
        "escape" => GHOSTTY_KEY_ESCAPE,
        "backspace" => GHOSTTY_KEY_BACKSPACE,
        "space" => return Some((GHOSTTY_KEY_SPACE, false, Some(' '))),
        "delete" => GHOSTTY_KEY_DELETE,
        "insert" => GHOSTTY_KEY_INSERT,
        "home" => GHOSTTY_KEY_HOME,
        "end" => GHOSTTY_KEY_END,
        "pageup" => GHOSTTY_KEY_PAGE_UP,
        "pagedown" => GHOSTTY_KEY_PAGE_DOWN,
        "up" => GHOSTTY_KEY_ARROW_UP,
        "down" => GHOSTTY_KEY_ARROW_DOWN,
        "left" => GHOSTTY_KEY_ARROW_LEFT,
        "right" => GHOSTTY_KEY_ARROW_RIGHT,
        _ => GHOSTTY_KEY_UNIDENTIFIED,
    };
    if named != GHOSTTY_KEY_UNIDENTIFIED {
        return Some((named, false, None));
    }
    if let Some(n) = key.strip_prefix('f').and_then(|n| n.parse::<u8>().ok())
        && (1..=12).contains(&n)
    {
        return Some((GHOSTTY_KEY_F1 + (n - 1) as GhosttyKey, false, None));
    }
    let mut chars = key.chars();
    let (Some(c), None) = (chars.next(), chars.next()) else {
        return None;
    };
    const SHIFTED: &[(char, char)] = &[
        ('~', '`'),
        ('!', '1'),
        ('@', '2'),
        ('#', '3'),
        ('$', '4'),
        ('%', '5'),
        ('^', '6'),
        ('&', '7'),
        ('*', '8'),
        ('(', '9'),
        (')', '0'),
        ('_', '-'),
        ('+', '='),
        ('{', '['),
        ('}', ']'),
        ('|', '\\'),
        (':', ';'),
        ('"', '\''),
        ('<', ','),
        ('>', '.'),
        ('?', '/'),
    ];
    let (base, shifted) = match SHIFTED.iter().find(|(shifted, _)| *shifted == c) {
        Some((_, base)) => (*base, true),
        None => (c, false),
    };
    let code = match base {
        'a'..='z' => GHOSTTY_KEY_A + (base as u8 - b'a') as GhosttyKey,
        '0'..='9' => GHOSTTY_KEY_DIGIT_0 + (base as u8 - b'0') as GhosttyKey,
        '`' => GHOSTTY_KEY_BACKQUOTE,
        '\\' => GHOSTTY_KEY_BACKSLASH,
        '[' => GHOSTTY_KEY_BRACKET_LEFT,
        ']' => GHOSTTY_KEY_BRACKET_RIGHT,
        ',' => GHOSTTY_KEY_COMMA,
        '=' => GHOSTTY_KEY_EQUAL,
        '-' => GHOSTTY_KEY_MINUS,
        '.' => GHOSTTY_KEY_PERIOD,
        '\'' => GHOSTTY_KEY_QUOTE,
        ';' => GHOSTTY_KEY_SEMICOLON,
        '/' => GHOSTTY_KEY_SLASH,
        _ => GHOSTTY_KEY_UNIDENTIFIED,
    };
    (code != GHOSTTY_KEY_UNIDENTIFIED).then_some((code, shifted, Some(base)))
}

impl Drop for Vt {
    fn drop(&mut self) {
        unsafe {
            ghostty_key_event_free(self.key_event);
            ghostty_key_encoder_free(self.keys);
            ghostty_render_state_row_cells_free(self.cells_it);
            ghostty_render_state_row_iterator_free(self.rows_it);
            ghostty_terminal_free(self.term);
            ghostty_render_state_free(self.shared.state);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(snapshot: &Snapshot, x: usize, y: usize) -> &Cell {
        &snapshot.cells[y][x]
    }

    #[test]
    fn red_text_lands_in_the_first_cells() {
        let mut vt = Vt::new(16, 4);
        vt.write(b"\x1b[31mhi");
        let snapshot = vt.snapshot("t", "/", false);
        assert_eq!(cell(&snapshot, 0, 0).text, "h");
        assert_eq!(cell(&snapshot, 1, 0).text, "i");
        let fg = cell(&snapshot, 0, 0).fg;
        assert!(fg.0 > fg.1 && fg.0 > fg.2, "{fg:?}");
    }

    #[test]
    fn a_device_attributes_query_is_answered() {
        let mut vt = Vt::new(16, 4);
        vt.write(b"\x1b[c");
        let reply = vt.take_replies();
        assert!(
            reply.windows(2).any(|pair| pair == [0x1b, b'[']),
            "reply {reply:?}"
        );
    }

    #[test]
    fn synchronized_output_holds_the_frame_until_release() {
        let mut vt = Vt::new(16, 4);
        vt.write(b"A");
        assert_eq!(cell(&vt.snapshot("t", "/", false), 0, 0).text, "A");
        vt.write(b"\x1b[?2026h");
        vt.write(b"B");
        assert_eq!(
            cell(&vt.snapshot("t", "/", false), 1, 0).text,
            "",
            "the held frame stays on A"
        );
        vt.write(b"\x1b[?2026l");
        assert_eq!(cell(&vt.snapshot("t", "/", false), 1, 0).text, "B");
    }

    #[test]
    fn a_plain_letter_encodes_as_itself() {
        let mut vt = Vt::new(16, 4);
        let bytes = vt.encode_input(&KeyInput {
            key: "a".into(),
            key_char: Some("a".into()),
            shift: false,
            control: false,
            alt: false,
        });
        assert_eq!(bytes, b"a");
    }
}
