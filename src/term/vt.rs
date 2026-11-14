//! libghostty-vt on one thread: bytes in, a cell grid out, query replies back.

use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ghostty_sys::*;

const SYNC_OUTPUT: GhosttyMode = 2026;
const GRAPHEME_CLUSTER: GhosttyMode = 2027;
pub(super) const MAX_NOTIFICATIONS: usize = 128;
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
    /// Background for the mouse selection highlight.
    pub selection: Rgb,
    pub palette: [Rgb; 16],
}

impl Default for TermColors {
    fn default() -> Self {
        Self {
            fg: Rgb(0xdd, 0xdd, 0xdd),
            bg: Rgb(0x1e, 0x1e, 0x1e),
            cursor: Rgb(0xf5, 0xf5, 0xf5),
            selection: Rgb(0x3a, 0x5f, 0x8a),
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

/// One step of a mouse selection gesture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectStep {
    Press,
    Drag,
    Release,
}

/// Keyboard modifiers held during a mouse event.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MouseMods {
    pub shift: bool,
    pub control: bool,
    pub alt: bool,
}

/// A mouse event for a program that tracks the mouse.
#[derive(Clone, Copy, Debug)]
pub struct MouseInput {
    pub action: MouseAction,
    pub button: Option<MouseButton>,
    pub x: f32,
    pub y: f32,
    pub cell_w: f32,
    pub cell_h: f32,
    pub any_pressed: bool,
    pub mods: MouseMods,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseAction {
    Press,
    Release,
    Motion,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    WheelUp,
    WheelDown,
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
    /// The title the program set with OSC 0/2, empty when it set none.
    pub title: String,
    pub exited: bool,
    pub spawn_error: Option<String>,
    /// The program asked for mouse reports (click goes to it unless Shift).
    pub mouse_tracking: bool,
    /// Find bar results, present only while a search has a needle.
    pub search: Option<SearchView>,
}

/// One visible search match: a run of cells on one viewport row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SearchHit {
    pub row: u16,
    /// First cell of the run (inclusive).
    pub from: u16,
    /// Cell after the run (exclusive).
    pub to: u16,
    /// Whether this run belongs to the match the find bar selected.
    pub selected: bool,
}

/// What the find bar reads from a terminal: how many matches there are, which
/// one is selected, and the visible runs to highlight.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchView {
    /// The query these results answer. The find bar compares it with what it
    /// sent, because a frame can cross from the session thread before the
    /// results for the newest keystroke exist.
    pub needle: String,
    /// Matches on the active screen, scrollback included.
    pub total: usize,
    /// 0-based index of the selected match, newest first.
    pub selected: Option<usize>,
    /// Viewport rows and columns to paint under the text.
    pub hits: Vec<SearchHit>,
}

/// The SGR state an ANSI replay has written so far. A run of cells that share
/// it costs one byte each instead of a full escape sequence per cell.
#[derive(Clone, Copy, PartialEq, Eq)]
struct ReplayStyle {
    bold: bool,
    faint: bool,
    italic: bool,
    underline: bool,
    strike: bool,
    fg: Rgb,
    bg: Option<Rgb>,
}

impl ReplayStyle {
    fn of(cell: &Cell) -> Self {
        Self {
            bold: cell.bold,
            faint: cell.faint,
            italic: cell.italic,
            underline: cell.underline,
            strike: cell.strike,
            fg: cell.fg,
            bg: cell.bg,
        }
    }

    fn write(self, out: &mut String) {
        use std::fmt::Write as _;

        out.push_str("\x1b[0m");
        if self.bold {
            out.push_str("\x1b[1m");
        }
        if self.faint {
            out.push_str("\x1b[2m");
        }
        if self.italic {
            out.push_str("\x1b[3m");
        }
        if self.underline {
            out.push_str("\x1b[4m");
        }
        if self.strike {
            out.push_str("\x1b[9m");
        }
        let _ = write!(out, "\x1b[38;2;{};{};{}m", self.fg.0, self.fg.1, self.fg.2);
        match self.bg {
            Some(bg) => {
                let _ = write!(out, "\x1b[48;2;{};{};{}m", bg.0, bg.1, bg.2);
            }
            None => out.push_str("\x1b[49m"),
        }
    }
}

impl Snapshot {
    /// What the program calls itself: its OSC title (an agent TUI names itself
    /// there), else the command it was launched as, else an empty string.
    /// Titles are untrusted text the program wrote, so control characters are
    /// dropped before the label leaves the terminal.
    pub fn label(&self) -> String {
        let title: String = self.title.chars().filter(|ch| !ch.is_control()).collect();
        let title = title.trim();
        if !title.is_empty() {
            return title.to_string();
        }
        self.command.clone()
    }

    pub fn blank(command: &str, cwd: &str) -> Self {
        Self::blank_with(command, cwd, TermColors::default().bg)
    }

    pub fn blank_with(command: &str, cwd: &str, bg: Rgb) -> Self {
        Self {
            rows: 24,
            bg,
            cells: Vec::new(),
            all: true,
            cursor: None,
            cursor_cell: None,
            command: command.to_string(),
            cwd: cwd.to_string(),
            title: String::new(),
            exited: false,
            spawn_error: None,
            mouse_tracking: false,
            search: None,
        }
    }

    /// A self-contained ANSI screen for a terminal that joined mid-stream.
    ///
    /// This is the viewport only: scrollback stays on the Mac. The caller
    /// supplies the modes that are not part of a cell grid (alternate screen,
    /// bracketed paste), so the phone's encoder starts in the same state.
    ///
    /// Style is written only where it changes and trailing blank cells are
    /// left to the clear above, which keeps a full screen inside one protocol
    /// frame. The second column of a wide character is not written: the glyph
    /// already advanced the phone's cursor past it.
    pub fn ansi_replay(&self, alt_screen: bool, bracketed_paste: bool) -> Vec<u8> {
        self.ansi_replay_with_history(&[], alt_screen, bracketed_paste)
    }

    /// An ANSI replay with `history`, the rows scrolled above the viewport
    /// (oldest first), so the phone can scroll back through them too.
    ///
    /// The history rows are printed as ordinary lines first, which puts them
    /// in the phone's scrollback; the viewport rows follow as the screen. The
    /// alternate screen has no scrollback, so it always replays the viewport
    /// alone.
    pub fn ansi_replay_with_history(
        &self,
        history: &[Arc<Vec<Cell>>],
        alt_screen: bool,
        bracketed_paste: bool,
    ) -> Vec<u8> {
        use std::fmt::Write as _;

        let mut out = String::new();
        if alt_screen {
            out.push_str("\x1b[?1049h");
        } else {
            out.push_str("\x1b[?1049l");
        }
        if bracketed_paste {
            out.push_str("\x1b[?2004h");
        } else {
            out.push_str("\x1b[?2004l");
        }
        if self.mouse_tracking {
            out.push_str("\x1b[?1000h\x1b[?1006h");
        } else {
            out.push_str("\x1b[?1000l\x1b[?1006l");
        }
        // Clear the screen and the old scrollback: this terminal may have
        // shown another pane, or the same pane before a reattach.
        out.push_str("\x1b[0m\x1b[3J\x1b[2J\x1b[H");
        let mut current: Option<ReplayStyle> = None;
        if !alt_screen && !history.is_empty() {
            // Lines printed one after another leave the earlier ones in the
            // scrollback; the viewport rows then land on the screen.
            for cells in history {
                write_replay_row(&mut out, cells, &mut current);
                out.push_str("\x1b[0m\r\n");
                current = None;
            }
            for (row, cells) in self.cells.iter().enumerate() {
                write_replay_row(&mut out, cells, &mut current);
                if row + 1 < self.cells.len() {
                    out.push_str("\x1b[0m\r\n");
                    current = None;
                }
            }
        } else {
            for (row, cells) in self.cells.iter().enumerate() {
                // Everything past the last painted cell is already blank.
                let width = replay_width(cells);
                if width == 0 {
                    continue;
                }
                let _ = write!(out, "\x1b[{};1H", row + 1);
                write_replay_cells(&mut out, cells, width, &mut current);
            }
        }
        out.push_str("\x1b[0m");
        match self.cursor {
            Some(cursor) => {
                let _ = write!(out, "\x1b[?25h\x1b[{};{}H", cursor.y + 1, cursor.x + 1);
            }
            None => out.push_str("\x1b[?25l"),
        }
        out.into_bytes()
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

/// The cells of a row up to its last painted cell: everything past it is
/// already blank.
fn replay_width(cells: &[Cell]) -> usize {
    cells
        .iter()
        .rposition(|cell| {
            !cell.text.is_empty() || cell.bg.is_some() || cell.underline || cell.strike
        })
        .map_or(0, |last| last + 1)
}

/// Writes one whole row as a line (no cursor positioning, no line break).
fn write_replay_row(out: &mut String, cells: &[Cell], current: &mut Option<ReplayStyle>) {
    let width = replay_width(cells);
    if width == 0 {
        return;
    }
    write_replay_cells(out, cells, width, current);
}

/// Writes the first `width` cells of a row, styling only where it changes.
fn write_replay_cells(
    out: &mut String,
    cells: &[Cell],
    width: usize,
    current: &mut Option<ReplayStyle>,
) {
    let mut spacer = false;
    for cell in cells.iter().take(width) {
        if spacer {
            spacer = false;
            continue;
        }
        spacer = cell.wide;
        let style = ReplayStyle::of(cell);
        if *current != Some(style) {
            style.write(out);
            *current = Some(style);
        }
        if cell.text.is_empty() {
            out.push(' ');
            // A wide cell whose glyph is hidden still holds two columns; the
            // spacer is skipped like any other.
            if cell.wide {
                out.push(' ');
            }
        } else {
            out.push_str(&cell.text);
        }
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

/// A terminal's explicit request for the user's attention.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TermNotification {
    pub title: String,
    pub body: String,
}

struct Shared {
    state: GhosttyRenderState,
    size: GhosttySizeReportSize,
    held_since: Option<Instant>,
    replies: Vec<u8>,
    notifications: VecDeque<TermNotification>,
}

fn ok(status: GhosttyResult, what: &str) {
    assert!(status == GHOSTTY_SUCCESS, "ghostty {what} failed: {status}");
}

/// Copy a borrowed string field out of a terminal. The pointer libghostty
/// returns is only valid until the next mutating call, so it is read here.
unsafe fn terminal_text(term: GhosttyTerminal, data: GhosttyTerminalData) -> String {
    unsafe {
        let mut out = GhosttyString::default();
        if ghostty_terminal_get(term, data, &mut out as *mut _ as *mut c_void) != GHOSTTY_SUCCESS
            || out.ptr.is_null()
            || out.len == 0
        {
            return String::new();
        }
        String::from_utf8_lossy(std::slice::from_raw_parts(out.ptr, out.len)).into_owned()
    }
}

/// Turn a client-reported pwd into a path. OSC 7 carries a `file://host/path`
/// URI; OSC 9 and iTerm2's OSC 1337 carry a bare path.
fn reported_path(raw: &str) -> Option<String> {
    let path = match raw.strip_prefix("file://") {
        Some(rest) => match rest.find('/') {
            Some(slash) => &rest[slash..],
            None => "/",
        },
        None => raw,
    };
    let path = percent_decode(path);
    (!path.is_empty()).then_some(path)
}

fn percent_decode(text: &str) -> String {
    fn hex(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        }
    }
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let decoded = (bytes[i] == b'%' && i + 2 < bytes.len())
            .then(|| hex(bytes[i + 1]).zip(hex(bytes[i + 2])))
            .flatten();
        match decoded {
            Some((high, low)) => {
                out.push(high * 16 + low);
                i += 3;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
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

// Ghostty invokes effects synchronously on the terminal's owning thread.
unsafe extern "C" fn on_desktop_notification(
    _term: GhosttyTerminal,
    userdata: *mut c_void,
    notification: *const GhosttyTerminalDesktopNotification,
) {
    // The sized request and its strings are borrowed only for this callback.
    let Some(notification) = (unsafe { notification.as_ref() }) else {
        return;
    };
    let text = |value: GhosttyString| {
        if value.ptr.is_null() || value.len == 0 {
            String::new()
        } else {
            // Limit untrusted terminal content before copying it into the UI.
            let bytes = unsafe { std::slice::from_raw_parts(value.ptr, value.len.min(4096)) };
            String::from_utf8_lossy(bytes)
                .chars()
                .filter(|ch| !ch.is_control() || *ch == '\n')
                .collect()
        }
    };
    let shared = unsafe { &mut *(userdata as *mut Shared) };
    if shared.notifications.len() == MAX_NOTIFICATIONS {
        shared.notifications.pop_front();
    }
    shared.notifications.push_back(TermNotification {
        title: text(notification.title),
        body: text(notification.body),
    });
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

/// A cached row: its cells plus the row-local selection they were read with.
type SeenRow = (Arc<Vec<Cell>>, Option<(u16, u16)>);

/// DEC private mode 2004. The C header spells it `GHOSTTY_MODE_BRACKETED_PASTE`,
/// but that macro is built by an inline helper bindgen does not emit.
const MODE_BRACKETED_PASTE: GhosttyMode = 2004;

/// `GHOSTTY_STYLE_FLAG_*` and the underline field are macros. Bindgen skips them.
const STYLE_BOLD: GhosttyStyleFlags = 1 << 0;
const STYLE_ITALIC: GhosttyStyleFlags = 1 << 1;
const STYLE_FAINT: GhosttyStyleFlags = 1 << 2;
const STYLE_INVERSE: GhosttyStyleFlags = 1 << 4;
const STYLE_INVISIBLE: GhosttyStyleFlags = 1 << 5;
const STYLE_STRIKE: GhosttyStyleFlags = 1 << 6;
const STYLE_UNDERLINE_MASK: GhosttyStyleFlags = 0x7 << 8;

pub struct Vt {
    term: GhosttyTerminal,
    shared: Box<Shared>,
    rows_it: GhosttyRenderStateRowIterator,
    cells_it: GhosttyRenderStateRowCells,
    keys: GhosttyKeyEncoder,
    key_event: GhosttyKeyEvent,
    mouse: GhosttyMouseEncoder,
    mouse_event: GhosttyMouseEvent,
    last_mouse_report: Vec<u8>,
    gesture: GhosttySelectionGesture,
    press: GhosttySelectionGestureEvent,
    drag: GhosttySelectionGestureEvent,
    release: GhosttySelectionGestureEvent,
    /// Created on the first search and kept until the terminal is freed.
    /// `needle` is empty when no search is running.
    search: Option<GhosttySearch>,
    needle: String,
    cols: u16,
    rows: u16,
    colors: TermColors,
    /// Rows read last frame by libghostty's row id, with their selection range.
    seen: HashMap<[u64; 2], SeenRow>,
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
            let mut mouse = std::ptr::null_mut();
            ok(
                ghostty_mouse_encoder_new(std::ptr::null(), &mut mouse),
                "mouse_encoder_new",
            );
            let mut mouse_event = std::ptr::null_mut();
            ok(
                ghostty_mouse_event_new(std::ptr::null(), &mut mouse_event),
                "mouse_event_new",
            );
            let mut gesture = std::ptr::null_mut();
            ok(
                ghostty_selection_gesture_new(std::ptr::null(), &mut gesture),
                "selection_gesture_new",
            );
            let gesture_event = |kind| {
                let mut event = std::ptr::null_mut();
                ok(
                    ghostty_selection_gesture_event_new(std::ptr::null(), &mut event, kind),
                    "gesture_event_new",
                );
                event
            };
            let (press, drag, release) = (
                gesture_event(GHOSTTY_SELECTION_GESTURE_EVENT_TYPE_PRESS),
                gesture_event(GHOSTTY_SELECTION_GESTURE_EVENT_TYPE_DRAG),
                gesture_event(GHOSTTY_SELECTION_GESTURE_EVENT_TYPE_RELEASE),
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
                notifications: VecDeque::new(),
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
            let notification: GhosttyTerminalDesktopNotificationFn = Some(on_desktop_notification);
            ghostty_terminal_set(
                term,
                GHOSTTY_TERMINAL_OPT_DESKTOP_NOTIFICATION,
                std::mem::transmute::<GhosttyTerminalDesktopNotificationFn, *const c_void>(
                    notification,
                ),
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
                mouse,
                mouse_event,
                last_mouse_report: Vec::new(),
                gesture,
                press,
                drag,
                release,
                search: None,
                needle: String::new(),
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

    pub fn take_notification(&mut self) -> Option<TermNotification> {
        self.shared.notifications.pop_front()
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
            // Read these first: the strings are borrowed from the terminal and
            // copied before the render-state calls below.
            let title = terminal_text(self.term, GHOSTTY_TERMINAL_DATA_TITLE);
            let pwd = terminal_text(self.term, GHOSTTY_TERMINAL_DATA_PWD);
            // Match endpoints must be read before anything below touches the
            // terminal; they are snapshots valid only for this frame.
            let search = self.search_view();
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
                    None => self.read_row_cells(fg_default, selection),
                };
                if known {
                    seen.insert(id.bits, (Arc::clone(&row), selection));
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
                cells: rows,
                all,
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
                row.push(self.read_cell(fg_default, in_sel));
                x = x.saturating_add(1);
            }
            row.resize(self.cols as usize, blank_cell(fg_default));
            Arc::new(row)
        }
    }

    unsafe fn read_cell(&mut self, fg_default: Rgb, selected: bool) -> Cell {
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
                let inverted = bg.unwrap_or(self.colors.bg);
                bg = Some(fg);
                fg = inverted;
            }
            if selected {
                bg = Some(self.colors.selection);
            }
            let mut text = String::new();
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
                    let mut cps = vec![0u32; len as usize];
                    let mut graphemes = GhosttyCodepointBuffer {
                        ptr: cps.as_mut_ptr(),
                        cap: cps.len(),
                        len: 0,
                    };
                    ghostty_render_state_row_cells_get(
                        cells,
                        GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_BUF,
                        &mut graphemes as *mut _ as *mut c_void,
                    );
                    let written = graphemes.len.min(cps.len());
                    text = cps
                        .into_iter()
                        .take(written)
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
            }
        }
    }

    pub fn mouse_tracking(&self) -> bool {
        let mut value = false;
        unsafe {
            ghostty_terminal_get(
                self.term,
                GHOSTTY_TERMINAL_DATA_MOUSE_TRACKING,
                &mut value as *mut _ as *mut c_void,
            );
        }
        value
    }

    pub fn alt_screen(&self) -> bool {
        let mut screen: GhosttyTerminalScreen = GHOSTTY_TERMINAL_SCREEN_PRIMARY;
        unsafe {
            ghostty_terminal_get(
                self.term,
                GHOSTTY_TERMINAL_DATA_ACTIVE_SCREEN,
                &mut screen as *mut _ as *mut c_void,
            );
        }
        screen == GHOSTTY_TERMINAL_SCREEN_ALTERNATE
    }

    pub fn scroll_viewport(&mut self, delta: isize) {
        let mut scroll = GhosttyTerminalScrollViewport {
            tag: GHOSTTY_TERMINAL_SCROLL_VIEWPORT_DELTA,
            ..Default::default()
        };
        scroll.value.delta = delta;
        unsafe { ghostty_terminal_scroll_viewport(self.term, scroll) };
    }

    /// Where the viewport sits in the scrollable area, when the terminal
    /// reports it.
    pub fn scrollbar(&mut self) -> Option<GhosttyTerminalScrollbar> {
        let mut bar = GhosttyTerminalScrollbar::default();
        let result = unsafe {
            ghostty_terminal_get(
                self.term,
                GHOSTTY_TERMINAL_DATA_SCROLLBAR,
                &mut bar as *mut _ as *mut c_void,
            )
        };
        (result == GHOSTTY_SUCCESS).then_some(bar)
    }

    /// Move the viewport to an absolute row of the scrollable area. Row 0 is
    /// the oldest scrollback row; the library clamps the value.
    pub fn scroll_to_row(&mut self, row: usize) {
        let mut scroll = GhosttyTerminalScrollViewport {
            tag: GHOSTTY_TERMINAL_SCROLL_VIEWPORT_ROW,
            ..Default::default()
        };
        scroll.value.row = row;
        unsafe { ghostty_terminal_scroll_viewport(self.term, scroll) };
    }

    /// The last `limit` rows above the viewport, oldest first: what a remote
    /// viewer needs to scroll back without its own copy of the session.
    ///
    /// The rows are read by paging the viewport up, so the caller's scroll
    /// position is put back before this returns. The alternate screen has no
    /// scrollback; it gives none.
    pub fn history_cells(&mut self, limit: usize) -> Vec<Arc<Vec<Cell>>> {
        if limit == 0 || self.alt_screen() {
            return Vec::new();
        }
        let Some(bar) = self.scrollbar() else {
            return Vec::new();
        };
        let view = bar.len as usize;
        let bottom = (bar.total as usize).saturating_sub(view);
        let want = bottom.min(limit);
        if want == 0 {
            return Vec::new();
        }
        let saved = bar.offset as usize;
        let start = bottom - want;
        let state = self.shared.state;
        let mut rows = Vec::with_capacity(want);
        let mut at = start;
        unsafe {
            while at < bottom {
                self.scroll_to_row(at);
                ghostty_render_state_update(state, self.term);
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
                ghostty_render_state_get(
                    state,
                    GHOSTTY_RENDER_STATE_DATA_ROW_ITERATOR,
                    &mut self.rows_it as *mut _ as *mut c_void,
                );
                let mut page = Vec::with_capacity(view);
                while ghostty_render_state_row_iterator_next(self.rows_it) {
                    page.push(self.read_row_cells(fg_default, None));
                }
                let take = (bottom - at).min(page.len());
                if take == 0 {
                    break;
                }
                rows.extend(page.into_iter().take(take));
                at += take;
            }
            self.scroll_to_row(saved);
            ghostty_render_state_update(state, self.term);
        }
        rows
    }

    /// Whether the terminal currently owns a selection.
    pub fn has_selection(&self) -> bool {
        let mut selection = GhosttySelection::default();
        unsafe {
            ghostty_terminal_get(
                self.term,
                GHOSTTY_TERMINAL_DATA_SELECTION,
                &mut selection as *mut _ as *mut c_void,
            ) == GHOSTTY_SUCCESS
        }
    }

    /// Clear the selection, reporting whether there was one. Callers draw only
    /// on a change, so typing in a terminal with nothing selected must not
    /// produce a new snapshot.
    pub fn clear_selection(&mut self) -> bool {
        if !self.has_selection() {
            return false;
        }
        unsafe {
            ghostty_terminal_set(self.term, GHOSTTY_TERMINAL_OPT_SELECTION, std::ptr::null());
        }
        true
    }

    /// Selected plain text, if any.
    pub fn selection_text(&self) -> Option<String> {
        unsafe {
            let mut selection = GhosttySelection::default();
            if ghostty_terminal_get(
                self.term,
                GHOSTTY_TERMINAL_DATA_SELECTION,
                &mut selection as *mut _ as *mut c_void,
            ) != GHOSTTY_SUCCESS
            {
                return None;
            }
            let mut formatter = std::ptr::null_mut();
            if ghostty_formatter_terminal_new(std::ptr::null(), &mut formatter, self.term)
                != GHOSTTY_SUCCESS
            {
                return None;
            }
            let emit = GHOSTTY_FORMATTER_FORMAT_PLAIN;
            let unwrap = true;
            let trim = true;
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
            (!text.is_empty()).then_some(text)
        }
    }

    /// Set the find bar's needle and jump to the newest match. An empty needle
    /// closes the search.
    pub fn search_set(&mut self, needle: &str) {
        if needle.is_empty() {
            self.search_clear();
            return;
        }
        let Some(search) = self.ensure_search() else {
            return;
        };
        let value = GhosttyString {
            ptr: needle.as_ptr(),
            len: needle.len(),
        };
        let status = unsafe {
            ghostty_search_set(
                search,
                GHOSTTY_SEARCH_OPT_NEEDLE,
                &value as *const _ as *const c_void,
            )
        };
        if status != GHOSTTY_SUCCESS {
            return;
        }
        self.needle = needle.to_string();
        unsafe {
            let _ = ghostty_search_run(search);
            // Typing a query lands on the newest match, like Ghostty's find
            // bar; Enter from there walks toward older content.
            let _ = ghostty_search_select_next(search);
        }
    }

    /// Move the selected match toward older (`older`) or newer content.
    pub fn search_step(&mut self, older: bool) -> bool {
        if self.needle.is_empty() {
            return false;
        }
        let Some(search) = self.search else {
            return false;
        };
        let status = unsafe {
            if older {
                ghostty_search_select_next(search)
            } else {
                ghostty_search_select_prev(search)
            }
        };
        status == GHOSTTY_SUCCESS
    }

    pub fn search_clear(&mut self) -> bool {
        if self.needle.is_empty() {
            return false;
        }
        self.needle.clear();
        if let Some(search) = self.search {
            unsafe {
                ghostty_search_set(search, GHOSTTY_SEARCH_OPT_NEEDLE, std::ptr::null());
            }
        }
        true
    }

    /// Let the search see terminal changes since the last frame. No-op while
    /// no needle is set, so ordinary output costs nothing.
    pub fn search_feed(&mut self) {
        if self.needle.is_empty() {
            return;
        }
        if let Some(search) = self.search {
            unsafe {
                let _ = ghostty_search_feed(search);
            }
        }
    }

    fn ensure_search(&mut self) -> Option<GhosttySearch> {
        if let Some(search) = self.search {
            return Some(search);
        }
        let mut search = std::ptr::null_mut();
        unsafe {
            ok(
                ghostty_search_new(std::ptr::null(), &mut search, self.term),
                "search_new",
            )
        };
        // A successful call always yields a handle. Refuse to cache a null one
        // so a later query can try again instead of silently searching nothing.
        if search.is_null() {
            return None;
        }
        self.search = Some(search);
        Some(search)
    }

    /// Read the current search state and translate visible matches into
    /// viewport rows and columns for painting.
    fn search_view(&self) -> Option<SearchView> {
        if self.needle.is_empty() {
            return None;
        }
        let search = self.search?;
        unsafe {
            let mut total: usize = 0;
            if ghostty_search_get(
                search,
                GHOSTTY_SEARCH_DATA_TOTAL_MATCHES,
                &mut total as *mut _ as *mut c_void,
            ) != GHOSTTY_SUCCESS
            {
                return None;
            }
            let mut index: usize = 0;
            let selected = (ghostty_search_get(
                search,
                GHOSTTY_SEARCH_DATA_SELECTED_INDEX,
                &mut index as *mut _ as *mut c_void,
            ) == GHOSTTY_SUCCESS)
                .then_some(index);
            let mut selected_match = GhosttySelection::default();
            let has_selected = ghostty_search_get(
                search,
                GHOSTTY_SEARCH_DATA_SELECTED_MATCH,
                &mut selected_match as *mut _ as *mut c_void,
            ) == GHOSTTY_SUCCESS;

            // Query the required capacity first; the viewport list is a page
            // or two of matches, so one allocation is enough.
            let mut needed = GhosttySelectionBuffer {
                ptr: std::ptr::null_mut(),
                cap: 0,
                len: 0,
            };
            let status = ghostty_search_get(
                search,
                GHOSTTY_SEARCH_DATA_VIEWPORT_MATCHES,
                &mut needed as *mut _ as *mut c_void,
            );
            if needed.len == 0 {
                return Some(SearchView {
                    needle: self.needle.clone(),
                    total,
                    selected,
                    hits: Vec::new(),
                });
            }
            if status != GHOSTTY_SUCCESS && status != GHOSTTY_OUT_OF_SPACE {
                return Some(SearchView {
                    needle: self.needle.clone(),
                    total,
                    selected,
                    hits: Vec::new(),
                });
            }
            let mut matches = vec![GhosttySelection::default(); needed.len];
            let mut buffer = GhosttySelectionBuffer {
                ptr: matches.as_mut_ptr(),
                cap: matches.len(),
                len: 0,
            };
            if ghostty_search_get(
                search,
                GHOSTTY_SEARCH_DATA_VIEWPORT_MATCHES,
                &mut buffer as *mut _ as *mut c_void,
            ) != GHOSTTY_SUCCESS
            {
                return Some(SearchView {
                    needle: self.needle.clone(),
                    total,
                    selected,
                    hits: Vec::new(),
                });
            }
            matches.truncate(buffer.len.min(matches.len()));

            let rows = self.rows;
            let cols = self.cols;
            let mut hits = Vec::new();
            for selection in &matches {
                let mut ordered = GhosttySelection::default();
                if ghostty_terminal_selection_ordered(
                    self.term,
                    selection,
                    GHOSTTY_SELECTION_ORDER_FORWARD,
                    &mut ordered,
                ) != GHOSTTY_SUCCESS
                {
                    continue;
                }
                let (mut start, mut end) = (
                    GhosttyPointCoordinate::default(),
                    GhosttyPointCoordinate::default(),
                );
                if ghostty_terminal_point_from_grid_ref(
                    self.term,
                    &ordered.start,
                    GHOSTTY_POINT_TAG_VIEWPORT,
                    &mut start,
                ) != GHOSTTY_SUCCESS
                    || ghostty_terminal_point_from_grid_ref(
                        self.term,
                        &ordered.end,
                        GHOSTTY_POINT_TAG_VIEWPORT,
                        &mut end,
                    ) != GHOSTTY_SUCCESS
                {
                    continue;
                }
                if start.y >= rows as u32 || end.y >= rows as u32 {
                    continue;
                }
                let mut equal = false;
                let selected = has_selected
                    && ghostty_terminal_selection_equal(
                        self.term,
                        selection,
                        &selected_match,
                        &mut equal,
                    ) == GHOSTTY_SUCCESS
                    && equal;
                let last = end.x.min(cols.saturating_sub(1));
                let first = start.x.min(last);
                if start.y == end.y {
                    hits.push(SearchHit {
                        row: start.y as u16,
                        from: first,
                        to: last + 1,
                        selected,
                    });
                } else {
                    hits.push(SearchHit {
                        row: start.y as u16,
                        from: first,
                        to: cols,
                        selected,
                    });
                    for row in start.y + 1..end.y {
                        hits.push(SearchHit {
                            row: row as u16,
                            from: 0,
                            to: cols,
                            selected,
                        });
                    }
                    hits.push(SearchHit {
                        row: end.y as u16,
                        from: 0,
                        to: last + 1,
                        selected,
                    });
                }
            }
            Some(SearchView {
                needle: self.needle.clone(),
                total,
                selected,
                hits,
            })
        }
    }

    /// One mouse step of a selection gesture. Positions are in grid-local pixels.
    #[allow(clippy::too_many_arguments)]
    pub fn select(
        &mut self,
        step: SelectStep,
        col: u16,
        row: u16,
        x: f32,
        y: f32,
        cell_w: f32,
        height: f32,
    ) {
        unsafe {
            let mut grid = GhosttyGridRef::default();
            let mut point = GhosttyPoint {
                tag: GHOSTTY_POINT_TAG_VIEWPORT,
                ..Default::default()
            };
            point.value.coordinate = GhosttyPointCoordinate {
                x: col.min(self.cols.saturating_sub(1)),
                y: u32::from(row.min(self.rows.saturating_sub(1))),
            };
            if ghostty_terminal_grid_ref(self.term, point, &mut grid) != GHOSTTY_SUCCESS {
                return;
            }
            let pos = GhosttySurfacePosition {
                x: f64::from(x),
                y: f64::from(y),
            };
            let event = match step {
                SelectStep::Press => self.press,
                SelectStep::Drag => self.drag,
                SelectStep::Release => self.release,
            };
            ghostty_selection_gesture_event_set(
                event,
                GHOSTTY_SELECTION_GESTURE_EVENT_OPT_REF,
                &grid as *const _ as *const c_void,
            );
            ghostty_selection_gesture_event_set(
                event,
                GHOSTTY_SELECTION_GESTURE_EVENT_OPT_POSITION,
                &pos as *const _ as *const c_void,
            );
            match step {
                SelectStep::Press => {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_nanos() as u64)
                        .unwrap_or(0);
                    ghostty_selection_gesture_event_set(
                        event,
                        GHOSTTY_SELECTION_GESTURE_EVENT_OPT_TIME_NS,
                        &now as *const _ as *const c_void,
                    );
                    let interval = 500_000_000u64;
                    let distance = 4.0f64;
                    ghostty_selection_gesture_event_set(
                        event,
                        GHOSTTY_SELECTION_GESTURE_EVENT_OPT_REPEAT_INTERVAL_NS,
                        &interval as *const _ as *const c_void,
                    );
                    ghostty_selection_gesture_event_set(
                        event,
                        GHOSTTY_SELECTION_GESTURE_EVENT_OPT_REPEAT_DISTANCE,
                        &distance as *const _ as *const c_void,
                    );
                }
                SelectStep::Drag => {
                    let geometry = GhosttySelectionGestureGeometry {
                        columns: u32::from(self.cols),
                        cell_width: cell_w.max(1.0).round() as u32,
                        padding_left: 0,
                        screen_height: height.max(1.0).round() as u32,
                    };
                    ghostty_selection_gesture_event_set(
                        event,
                        GHOSTTY_SELECTION_GESTURE_EVENT_OPT_GEOMETRY,
                        &geometry as *const _ as *const c_void,
                    );
                }
                SelectStep::Release => {}
            }
            let mut selection = GhosttySelection::default();
            match ghostty_selection_gesture_event(self.gesture, self.term, event, &mut selection) {
                GHOSTTY_SUCCESS => {
                    ghostty_terminal_set(
                        self.term,
                        GHOSTTY_TERMINAL_OPT_SELECTION,
                        &selection as *const _ as *const c_void,
                    );
                }
                _ if matches!(step, SelectStep::Press) => {
                    ghostty_terminal_set(
                        self.term,
                        GHOSTTY_TERMINAL_OPT_SELECTION,
                        std::ptr::null(),
                    );
                }
                _ => {}
            }
        }
    }

    /// Encode a mouse event for a program that tracks the mouse.
    pub fn encode_mouse(&mut self, input: MouseInput) -> Vec<u8> {
        let action = match input.action {
            MouseAction::Press => GHOSTTY_MOUSE_ACTION_PRESS,
            MouseAction::Release => GHOSTTY_MOUSE_ACTION_RELEASE,
            MouseAction::Motion => GHOSTTY_MOUSE_ACTION_MOTION,
        };
        let button = input.button.map(|b| match b {
            MouseButton::Left => GHOSTTY_MOUSE_BUTTON_LEFT,
            MouseButton::Right => GHOSTTY_MOUSE_BUTTON_RIGHT,
            MouseButton::Middle => GHOSTTY_MOUSE_BUTTON_MIDDLE,
            MouseButton::WheelUp => GHOSTTY_MOUSE_BUTTON_FOUR,
            MouseButton::WheelDown => GHOSTTY_MOUSE_BUTTON_FIVE,
        });
        let (cw, ch) = (input.cell_w.round().max(1.0), input.cell_h.round().max(1.0));
        let (x, y) = (
            input.x / input.cell_w.max(1.0) * cw,
            input.y / input.cell_h.max(1.0) * ch,
        );
        unsafe {
            ghostty_mouse_encoder_set_from_terminal(self.mouse, self.term);
            let size = GhosttyMouseEncoderSize {
                size: std::mem::size_of::<GhosttyMouseEncoderSize>(),
                screen_width: u32::from(self.cols) * cw as u32,
                screen_height: u32::from(self.rows) * ch as u32,
                cell_width: cw as u32,
                cell_height: ch as u32,
                ..Default::default()
            };
            ghostty_mouse_encoder_set(
                self.mouse,
                GHOSTTY_MOUSE_ENCODER_OPT_SIZE,
                &size as *const _ as *const c_void,
            );
            ghostty_mouse_encoder_set(
                self.mouse,
                GHOSTTY_MOUSE_ENCODER_OPT_ANY_BUTTON_PRESSED,
                &input.any_pressed as *const _ as *const c_void,
            );
            let event = self.mouse_event;
            ghostty_mouse_event_set_action(event, action);
            match button {
                Some(b) => {
                    ghostty_mouse_event_set_button(event, b);
                }
                None => {
                    ghostty_mouse_event_clear_button(event);
                }
            }
            let mut mods: GhosttyMods = 0;
            if input.mods.shift {
                mods |= GHOSTTY_MODS_SHIFT as GhosttyMods;
            }
            if input.mods.control {
                mods |= GHOSTTY_MODS_CTRL as GhosttyMods;
            }
            if input.mods.alt {
                mods |= GHOSTTY_MODS_ALT as GhosttyMods;
            }
            ghostty_mouse_event_set_mods(event, mods);
            ghostty_mouse_event_set_position(event, GhosttyMousePosition { x, y });
            let mut buf = [0u8; 64];
            let mut len = 0usize;
            let status = ghostty_mouse_encoder_encode(
                self.mouse,
                event,
                buf.as_mut_ptr() as *mut _,
                buf.len(),
                &mut len,
            );
            if status != GHOSTTY_SUCCESS || len == 0 {
                return Vec::new();
            }
            if action == GHOSTTY_MOUSE_ACTION_MOTION && self.last_mouse_report == buf[..len] {
                return Vec::new();
            }
            self.last_mouse_report.clear();
            self.last_mouse_report.extend_from_slice(&buf[..len]);
            buf[..len].to_vec()
        }
    }

    /// Arrow-key scroll for alternate-screen programs that don't take the mouse.
    pub fn encode_scroll_keys(&mut self, up: bool, n: usize) -> Vec<u8> {
        let key = if up {
            GHOSTTY_KEY_ARROW_UP
        } else {
            GHOSTTY_KEY_ARROW_DOWN
        };
        let mut out = Vec::new();
        for _ in 0..n {
            out.extend(self.encode_key(key, 0, 0, None, None));
        }
        out
    }

    /// Whether the program asked for bracketed paste (DEC private mode 2004).
    pub fn bracketed_paste(&self) -> bool {
        let mut mode = GhosttyTerminalModeConfig {
            mode: MODE_BRACKETED_PASTE,
            value: false,
        };
        let ok = unsafe {
            ghostty_terminal_get(
                self.term,
                GHOSTTY_TERMINAL_DATA_MODE,
                &mut mode as *mut _ as *mut c_void,
            ) == GHOSTTY_SUCCESS
        };
        ok && mode.value
    }

    /// Pasting text for the PTY: control bytes stripped, wrapped in bracketed
    /// paste markers when the program asked for them.
    ///
    /// Without the markers a multi-line paste is indistinguishable from typing,
    /// so every newline is executed — which is why programs enable mode 2004.
    pub fn encode_paste(&self, text: &[u8]) -> Vec<u8> {
        let bracketed = self.bracketed_paste();
        let mut data = text.to_vec();
        // Bracketed paste adds `ESC[200~` and `ESC[201~`; everything else in
        // the encoding keeps the input length.
        let mut buf = vec![0u8; data.len() + 12];
        let mut written = 0usize;
        let mut status = unsafe {
            ghostty_paste_encode(
                data.as_mut_ptr() as *mut ::std::os::raw::c_char,
                data.len(),
                bracketed,
                buf.as_mut_ptr(),
                buf.len(),
                &mut written,
            )
        };
        if status == GHOSTTY_OUT_OF_SPACE {
            buf = vec![0u8; written];
            status = unsafe {
                ghostty_paste_encode(
                    data.as_mut_ptr() as *mut ::std::os::raw::c_char,
                    data.len(),
                    bracketed,
                    buf.as_mut_ptr(),
                    buf.len(),
                    &mut written,
                )
            };
        }
        if status != GHOSTTY_SUCCESS || written == 0 {
            return Vec::new();
        }
        buf.truncate(written);
        buf
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
            ghostty_key_encoder_set_from_terminal(self.keys, self.term);
            let event = self.key_event;
            ghostty_key_event_set_action(event, GHOSTTY_KEY_ACTION_PRESS);
            ghostty_key_event_set_key(event, key);
            ghostty_key_event_set_mods(event, mods);
            ghostty_key_event_set_consumed_mods(event, consumed);
            let utf8 = match text {
                Some(text) => GhosttyString {
                    ptr: text.as_ptr().cast(),
                    len: text.len(),
                },
                None => GhosttyString {
                    ptr: std::ptr::null(),
                    len: 0,
                },
            };
            ghostty_key_event_set_utf8(event, utf8);
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
            ghostty_selection_gesture_event_free(self.press);
            ghostty_selection_gesture_event_free(self.drag);
            ghostty_selection_gesture_event_free(self.release);
            ghostty_selection_gesture_free(self.gesture, self.term);
            ghostty_mouse_event_free(self.mouse_event);
            ghostty_mouse_encoder_free(self.mouse);
            ghostty_key_event_free(self.key_event);
            ghostty_key_encoder_free(self.keys);
            ghostty_render_state_row_cells_free(self.cells_it);
            ghostty_render_state_row_iterator_free(self.rows_it);
            if let Some(search) = self.search {
                ghostty_search_free(search);
            }
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
    fn terminal_notifications_parse_across_pty_reads() {
        let mut vt = Vt::new(80, 24);
        vt.write(b"\x1b]9;");
        assert_eq!(vt.take_notification(), None);
        vt.write("任务完成".as_bytes());
        // Ghostty dispatches OSC on ESC, before consuming ST's final slash.
        vt.write(b"\x1b");
        assert_eq!(
            vt.take_notification(),
            Some(TermNotification {
                title: String::new(),
                body: "任务完成".into(),
            })
        );
        vt.write(b"\\");
        assert_eq!(vt.take_notification(), None);

        vt.write(b"\x1b]777;notify;Claude;Permission needed\x07");
        assert_eq!(
            vt.take_notification(),
            Some(TermNotification {
                title: "Claude".into(),
                body: "Permission needed".into(),
            })
        );
        assert_eq!(
            vt.take_notification(),
            None,
            "OSC's BEL terminator is not another bell"
        );
    }

    /// Agent session updates ride the notification channel: the app reads the
    /// body prefix and records the id instead of showing it.
    #[test]
    fn a_session_update_keeps_its_agent_and_prefix() {
        let mut vt = Vt::new(80, 24);
        vt.write(b"\x1b]777;notify;codex;kumu-session:abc-1\x07");
        assert_eq!(
            vt.take_notification(),
            Some(TermNotification {
                title: "codex".into(),
                body: "kumu-session:abc-1".into(),
            })
        );
    }

    #[test]
    fn terminal_titles_progress_and_bells_do_not_request_attention() {
        let mut vt = Vt::new(80, 24);
        vt.write(b"\x1b]2;Terminal title\x07\x1b]9;4;1;50\x07");
        assert_eq!(vt.take_notification(), None);
        vt.write(b"\x07\x07");
        assert_eq!(vt.take_notification(), None);
        vt.write(b"\x1b]9;Ready\x07\x07");
        assert_eq!(
            vt.take_notification().expect("OSC notification").body,
            "Ready"
        );
        assert_eq!(
            vt.take_notification(),
            None,
            "a following bell must not create another alert"
        );
    }

    #[test]
    fn a_program_title_and_reported_pwd_reach_the_snapshot() {
        let mut vt = Vt::new(40, 4);
        vt.write(b"\x1b]2;Claude Code\x07");
        let snapshot = vt.snapshot("zsh", "/launch", false);
        assert_eq!(snapshot.title, "Claude Code");
        assert_eq!(snapshot.label(), "Claude Code");
        assert_eq!(snapshot.cwd, "/launch", "no OSC 7: the launch directory");

        // OSC 7 carries a URI and percent-encodes the path.
        vt.write(b"\x1b]7;file://host/Users/me/kumu%20demo\x07");
        let snapshot = vt.snapshot("zsh", "/launch", false);
        assert_eq!(snapshot.cwd, "/Users/me/kumu demo");

        // iTerm2's OSC 1337 carries a bare path.
        vt.write(b"\x1b]1337;CurrentDir=/tmp/demo\x07");
        assert_eq!(vt.snapshot("zsh", "/launch", false).cwd, "/tmp/demo");
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
        let reply = String::from_utf8_lossy(&reply);
        assert!(
            reply.starts_with("\x1b[?")
                && reply.ends_with('c')
                && reply[3..reply.len() - 1]
                    .split(';')
                    .all(|field| field.parse::<u16>().is_ok()),
            "DA1 reply {reply:?}"
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
    fn completion_keys_encode_for_the_shell() {
        let mut vt = Vt::new(16, 4);
        let mut input = KeyInput {
            key: "a".into(),
            key_char: Some("a".into()),
            shift: false,
            control: false,
            alt: false,
        };
        assert_eq!(vt.encode_input(&input), b"a", "a plain letter is itself");
        input.key = "tab".into();
        input.key_char = Some("\t".into());
        assert_eq!(vt.encode_input(&input), b"\t");
        input.shift = true;
        assert_eq!(vt.encode_input(&input), b"\x1b[Z");
    }

    #[test]
    fn a_drag_selects_plain_text() {
        let mut vt = Vt::new(16, 4);
        vt.write(b"hello");
        vt.select(SelectStep::Press, 0, 0, 1.0, 5.0, 10.0, 40.0);
        vt.select(SelectStep::Drag, 5, 0, 50.0, 5.0, 10.0, 40.0);
        vt.select(SelectStep::Release, 5, 0, 50.0, 5.0, 10.0, 40.0);
        assert_eq!(vt.selection_text().as_deref(), Some("hello"));
        assert!(vt.clear_selection(), "a selection was there to clear");
        assert_eq!(vt.selection_text(), None);
        assert!(!vt.clear_selection(), "clearing again changes nothing");
    }

    #[test]
    fn search_finds_matches_and_reports_their_viewport_rows() {
        let mut vt = Vt::new(20, 6);
        for line in [
            "first alpha",
            "beta",
            "second alpha",
            "gamma",
            "third alpha",
        ] {
            vt.write(line.as_bytes());
            vt.write(b"\r\n");
        }
        vt.search_set("alpha");
        let snapshot = vt.snapshot("sh", "/", false);
        let search = snapshot.search.clone().expect("a search with a needle");
        assert_eq!(search.total, 3);
        assert_eq!(search.selected, Some(0), "the newest match is selected");
        assert!(!search.hits.is_empty());

        let mut selected = 0;
        for hit in &search.hits {
            let row = &snapshot.cells[hit.row as usize];
            let text: String = row[hit.from as usize..hit.to as usize]
                .iter()
                .map(|cell| cell.text.as_str())
                .collect();
            assert_eq!(text, "alpha", "hit {hit:?} covers {text:?}");
            if hit.selected {
                selected += 1;
            }
        }
        assert_eq!(selected, 1, "one visible run is the selected match");
    }

    #[test]
    fn search_steps_toward_older_matches_and_wraps() {
        let mut vt = Vt::new(20, 6);
        for line in ["alpha one", "beta", "alpha two", "gamma", "alpha three"] {
            vt.write(line.as_bytes());
            vt.write(b"\r\n");
        }
        vt.search_set("alpha");
        let selected = |vt: &mut Vt| {
            vt.snapshot("sh", "/", false)
                .search
                .expect("search view")
                .selected
        };
        assert_eq!(selected(&mut vt), Some(0));
        assert!(vt.search_step(true));
        assert_eq!(selected(&mut vt), Some(1));
        assert!(vt.search_step(true));
        assert_eq!(selected(&mut vt), Some(2));
        assert!(vt.search_step(true), "after the oldest, wraps to newest");
        assert_eq!(selected(&mut vt), Some(0));
        assert!(vt.search_step(false));
        assert_eq!(selected(&mut vt), Some(2), "back toward newer content");
    }

    #[test]
    fn clearing_the_needle_removes_search_highlights() {
        let mut vt = Vt::new(20, 4);
        vt.write(b"alpha\r\n");
        vt.search_set("alpha");
        assert!(vt.snapshot("sh", "/", false).search.is_some());
        assert!(vt.search_clear());
        let snapshot = vt.snapshot("sh", "/", false);
        assert!(snapshot.search.is_none());
    }

    #[test]
    fn search_reads_scrollback_beyond_the_visible_rows() {
        let mut vt = Vt::new(20, 4);
        for line in 0..40 {
            vt.write(format!("line {line} alpha\r\n").as_bytes());
        }
        vt.search_set("alpha");
        let snapshot = vt.snapshot("sh", "/", false);
        let search = snapshot.search.expect("search view");
        assert_eq!(search.total, 40);
        for hit in &search.hits {
            assert!(hit.row < snapshot.rows, "hit {hit:?} outside the viewport");
        }
    }

    #[test]
    fn a_replay_keeps_a_wide_character_on_its_own_columns() {
        let fg = Rgb(0xdd, 0xdd, 0xdd);
        let mut cells = vec![blank_cell(fg); 4];
        cells[0].text = "中".into();
        cells[0].wide = true;
        // cells[1] is the wide character's spacer: still blank.
        cells[2].text = "a".into();
        let replay = Snapshot {
            rows: 1,
            bg: fg,
            cells: vec![Arc::new(cells)],
            all: true,
            cursor: None,
            cursor_cell: None,
            command: "sh".into(),
            cwd: "/".into(),
            title: String::new(),
            exited: false,
            spawn_error: None,
            mouse_tracking: false,
            search: None,
        }
        .ansi_replay(false, false);
        let text = String::from_utf8(replay).expect("utf-8 replay");
        assert!(
            text.contains("中a"),
            "the spacer after a wide character must not write a column: {text:?}"
        );
    }

    #[test]
    fn a_replay_styles_only_where_the_cell_changes() {
        let fg = Rgb(0xdd, 0xdd, 0xdd);
        let mut cells = vec![blank_cell(fg); 3];
        cells[0].text = "a".into();
        cells[1].text = "b".into();
        cells[2].text = "c".into();
        let replay = Snapshot {
            rows: 1,
            bg: fg,
            cells: vec![Arc::new(cells)],
            all: true,
            cursor: None,
            cursor_cell: None,
            command: "sh".into(),
            cwd: "/".into(),
            title: String::new(),
            exited: false,
            spawn_error: None,
            mouse_tracking: false,
            search: None,
        }
        .ansi_replay(false, false);
        let text = String::from_utf8(replay).expect("utf-8 replay");
        assert_eq!(
            text.matches("\x1b[38;2;").count(),
            1,
            "a run of identical cells shares one escape sequence: {text:?}"
        );
    }

    #[test]
    fn a_replay_rebuilds_the_same_screen() {
        let mut vt = Vt::new(20, 4);
        vt.write("echo 中文 ok\r\n".as_bytes());
        vt.write("\x1b[31mred\x1b[0m plain\r\n".as_bytes());
        let original = vt.snapshot("sh", "/", false);

        let mut phone = Vt::new(20, 4);
        phone.write(&original.ansi_replay(false, false));
        let restored = phone.snapshot("sh", "/", false);

        assert_eq!(restored.plain_text(), original.plain_text());
        assert_eq!(
            restored.cells, original.cells,
            "the phone's grid must match the Mac's cell for cell"
        );
    }

    #[test]
    fn a_replay_carries_the_scrollback() {
        let mut vt = Vt::new(20, 4);
        for line in 0..20 {
            vt.write(format!("line {line}\r\n").as_bytes());
        }
        let snapshot = vt.snapshot("sh", "/", false);
        let history = vt.history_cells(10);
        assert_eq!(history.len(), 10, "ten rows above the viewport");

        let mut phone = Vt::new(20, 4);
        phone.write(&snapshot.ansi_replay_with_history(&history, false, false));
        let restored = phone.snapshot("sh", "/", false);
        assert_eq!(restored.plain_text(), snapshot.plain_text());
        let bar = phone.scrollbar().expect("the phone has a scrollback");
        assert!(
            bar.total > bar.len,
            "the phone keeps the replayed history above the screen"
        );

        phone.scroll_to_row(0);
        let top = phone.snapshot("sh", "/", false);
        assert!(
            top.plain_text().contains("line 9"),
            "the oldest replayed row is in the phone's scrollback: {:?}",
            top.plain_text()
        );
    }

    #[test]
    fn an_alternate_screen_replay_has_no_scrollback() {
        let mut vt = Vt::new(20, 4);
        vt.write(b"\x1b[?1049h");
        for line in 0..20 {
            vt.write(format!("alt {line}\r\n").as_bytes());
        }
        let snapshot = vt.snapshot("sh", "/", false);
        let history = vt.history_cells(10);
        assert!(history.is_empty(), "the alternate screen keeps none");

        let mut phone = Vt::new(20, 4);
        phone.write(&snapshot.ansi_replay_with_history(&history, true, false));
        let bar = phone.scrollbar().expect("a scrollbar");
        assert_eq!(bar.total, bar.len, "nothing above the alternate screen");
    }

    /// A find bar restarts the search on every keystroke, so one search over a
    /// full scrollback must not block the session thread for long.
    #[test]
    fn searching_a_full_scrollback_stays_interactive() {
        let mut vt = Vt::new(100, 40);
        for line in 0..10_000 {
            vt.write(format!("line {line} with some words on it\r\n").as_bytes());
        }
        let started = std::time::Instant::now();
        vt.search_set("words");
        let elapsed = started.elapsed();
        let total = vt
            .snapshot("sh", "/", false)
            .search
            .expect("search view")
            .total;
        assert!(
            total > 0,
            "the scrollback holds matches, search found {total}"
        );
        // This guards against a pathological search, not a latency budget: a
        // loaded CI machine must not turn it into a flaky benchmark.
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "a keystroke waited {elapsed:?} for the search"
        );
    }

    #[test]
    fn a_paste_is_framed_when_the_program_asks_for_it() {
        let mut vt = Vt::new(16, 4);
        // No mode 2004: typing, so a newline is a carriage return.
        assert_eq!(vt.encode_paste(b"echo hi\n"), b"echo hi\r");
        vt.write(b"\x1b[?2004h");
        assert_eq!(
            vt.encode_paste(b"echo hi\nls\n"),
            b"\x1b[200~echo hi\nls\n\x1b[201~"
        );
    }

    #[test]
    fn a_mouse_report_carries_modifiers() {
        let mut vt = Vt::new(16, 4);
        vt.write(b"\x1b[?1002h\x1b[?1006h");
        let bytes = vt.encode_mouse(MouseInput {
            action: MouseAction::Press,
            button: Some(MouseButton::Left),
            x: 5.0,
            y: 5.0,
            cell_w: 10.0,
            cell_h: 20.0,
            any_pressed: true,
            mods: MouseMods {
                shift: false,
                control: true,
                alt: false,
            },
        });
        assert_eq!(bytes, b"\x1b[<16;1;1M");
    }
}
