//! libghostty-vt on one thread: bytes in, a cell grid out, query replies back.

mod input;
mod search;
mod snapshot;
#[cfg(test)]
mod tests;
mod types;

pub use types::*;

use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use ghostty_sys::*;

const SYNC_OUTPUT: GhosttyMode = 2026;
const GRAPHEME_CLUSTER: GhosttyMode = 2027;
pub(super) const MAX_NOTIFICATIONS: usize = 128;
const MAX_REPLIES_BYTES: usize = 64 * 1024;
const MAX_HYPERLINK_URI_BYTES: usize = 8192;
const MAX_HOLD: Duration = Duration::from_millis(1000);
const SCROLLBACK_MAX_LINES: usize = 10_000;
const SCROLLBACK_MAX_BYTES: usize = 10_000_000;

struct Shared {
    state: GhosttyRenderState,
    size: GhosttySizeReportSize,
    held_since: Option<Instant>,
    replies: Vec<u8>,
    notifications: VecDeque<TermNotification>,
}

/// Log a libghostty failure instead of panicking the terminal thread.
///
/// Construction paths check the handles they receive and surface a spawn error
/// to the pane; non-fatal configuration calls keep the terminal usable.
fn ok(status: GhosttyResult, what: &str) -> bool {
    if status == GHOSTTY_SUCCESS {
        true
    } else {
        eprintln!("[term] ghostty {what} failed: {status}");
        false
    }
}

/// A libghostty constructor that returned a null handle cannot be used; report
/// it to the pane instead of dereferencing it later on the terminal thread.
fn require_handle<T>(handle: *mut T, what: &str) -> Result<(), String> {
    if handle.is_null() {
        Err(format!("无法初始化终端（ghostty {what}）"))
    } else {
        Ok(())
    }
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

fn local_hostname() -> &'static str {
    static HOSTNAME: OnceLock<String> = OnceLock::new();
    HOSTNAME
        .get_or_init(|| {
            std::process::Command::new("/bin/hostname")
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .unwrap_or_default()
        })
        .as_str()
}

/// Turn a client-reported pwd into a path. OSC 7 carries a `file://host/path`
/// URI; OSC 9 and iTerm2's OSC 1337 carry a bare path.
fn reported_path(raw: &str) -> Option<String> {
    let path = match raw.strip_prefix("file://") {
        Some(rest) => {
            let (host, path) = match rest.find('/') {
                Some(slash) => (&rest[..slash], &rest[slash..]),
                None => (rest, "/"),
            };
            if !host.is_empty() && !host.eq_ignore_ascii_case("localhost") {
                let local = local_hostname();
                let short = local.split('.').next().unwrap_or("");
                let matches_local = !local.is_empty()
                    && (host.eq_ignore_ascii_case(local)
                        || (!short.is_empty() && host.eq_ignore_ascii_case(short)));
                if !matches_local {
                    return None;
                }
            }
            path
        }
        None => raw,
    };
    let path = percent_decode(path);
    if path.is_empty() || path.chars().any(|ch| ch == '\0' || ch.is_control()) {
        return None;
    }
    Some(path)
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

fn utf8_prefix(bytes: &[u8]) -> &[u8] {
    match std::str::from_utf8(bytes) {
        Ok(_) => bytes,
        Err(err) => {
            if err.error_len().is_none() {
                &bytes[..err.valid_up_to()]
            } else {
                bytes
            }
        }
    }
}

unsafe extern "C" fn on_write_pty(
    _term: GhosttyTerminal,
    userdata: *mut c_void,
    data: *const u8,
    len: usize,
) {
    if userdata.is_null() || data.is_null() || len == 0 {
        return;
    }
    let shared = unsafe { &mut *(userdata as *mut Shared) };
    let remaining = MAX_REPLIES_BYTES.saturating_sub(shared.replies.len());
    if remaining > 0 {
        let take = len.min(remaining);
        shared
            .replies
            .extend_from_slice(unsafe { std::slice::from_raw_parts(data, take) });
    }
}

// Ghostty invokes effects synchronously on the terminal's owning thread.
unsafe extern "C" fn on_desktop_notification(
    _term: GhosttyTerminal,
    userdata: *mut c_void,
    notification: *const GhosttyTerminalDesktopNotification,
) {
    if userdata.is_null() {
        return;
    }
    // The sized request and its strings are borrowed only for this callback.
    let Some(notification) = (unsafe { notification.as_ref() }) else {
        return;
    };
    let text = |value: GhosttyString| {
        if value.ptr.is_null() || value.len == 0 {
            String::new()
        } else {
            // Limit untrusted terminal content without splitting a UTF-8 sequence.
            let bytes = unsafe { std::slice::from_raw_parts(value.ptr, value.len.min(4096)) };
            let bytes = if value.len > 4096 {
                utf8_prefix(bytes)
            } else {
                bytes
            };
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
    if userdata.is_null() {
        return;
    }
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
    if userdata.is_null() || out.is_null() {
        return false;
    }
    let shared = unsafe { &*(userdata as *const Shared) };
    if shared.size.cell_width == 0 || shared.size.cell_height == 0 {
        return false;
    }
    unsafe { *out = shared.size };
    true
}

/// A cached row: its cells plus the row-local selection they were read with.
type SeenRow = (Arc<Vec<Cell>>, Option<(u16, u16)>);

pub struct Vt {
    term: GhosttyTerminal,
    /// Owned via raw pointer so Ghostty's synchronous callbacks can borrow
    /// `Shared` through `userdata` without violating `Box`'s `noalias` rules.
    shared: NonNull<Shared>,
    epoch: Instant,
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
    autoscroll: GhosttySelectionGestureEvent,
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
        Self::with_colors(cols, rows, TermColors::default()).expect("ghostty terminal")
    }

    pub fn with_colors(cols: u16, rows: u16, colors: TermColors) -> Result<Self, String> {
        unsafe {
            let cols = cols.max(1);
            let rows = rows.max(1);
            let mut term = std::ptr::null_mut();
            ok(
                ghostty_terminal_new(std::ptr::null(), &mut term, cols, rows),
                "terminal_new",
            );
            require_handle(term, "terminal_new")?;
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
            require_handle(state, "render_state_new")?;
            let mut rows_it = std::ptr::null_mut();
            ok(
                ghostty_render_state_row_iterator_new(std::ptr::null(), &mut rows_it),
                "row_iterator_new",
            );
            require_handle(rows_it, "row_iterator_new")?;
            let mut cells_it = std::ptr::null_mut();
            ok(
                ghostty_render_state_row_cells_new(std::ptr::null(), &mut cells_it),
                "row_cells_new",
            );
            require_handle(cells_it, "row_cells_new")?;
            let mut keys = std::ptr::null_mut();
            ok(
                ghostty_key_encoder_new(std::ptr::null(), &mut keys),
                "key_encoder_new",
            );
            require_handle(keys, "key_encoder_new")?;
            let mut key_event = std::ptr::null_mut();
            ok(
                ghostty_key_event_new(std::ptr::null(), &mut key_event),
                "key_event_new",
            );
            require_handle(key_event, "key_event_new")?;
            let mut mouse = std::ptr::null_mut();
            ok(
                ghostty_mouse_encoder_new(std::ptr::null(), &mut mouse),
                "mouse_encoder_new",
            );
            require_handle(mouse, "mouse_encoder_new")?;
            let mut mouse_event = std::ptr::null_mut();
            ok(
                ghostty_mouse_event_new(std::ptr::null(), &mut mouse_event),
                "mouse_event_new",
            );
            require_handle(mouse_event, "mouse_event_new")?;
            let mut gesture = std::ptr::null_mut();
            ok(
                ghostty_selection_gesture_new(std::ptr::null(), &mut gesture),
                "selection_gesture_new",
            );
            require_handle(gesture, "selection_gesture_new")?;
            let gesture_event = |kind| {
                let mut event = std::ptr::null_mut();
                ok(
                    ghostty_selection_gesture_event_new(std::ptr::null(), &mut event, kind),
                    "gesture_event_new",
                );
                event
            };
            let (press, drag, release, autoscroll) = (
                gesture_event(GHOSTTY_SELECTION_GESTURE_EVENT_TYPE_PRESS),
                gesture_event(GHOSTTY_SELECTION_GESTURE_EVENT_TYPE_DRAG),
                gesture_event(GHOSTTY_SELECTION_GESTURE_EVENT_TYPE_RELEASE),
                gesture_event(GHOSTTY_SELECTION_GESTURE_EVENT_TYPE_AUTOSCROLL_TICK),
            );
            require_handle(press, "gesture_event_new")?;
            require_handle(drag, "gesture_event_new")?;
            require_handle(release, "gesture_event_new")?;
            require_handle(autoscroll, "gesture_event_new")?;

            let shared = NonNull::new(Box::into_raw(Box::new(Shared {
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
            })))
            .expect("Box::into_raw returned null");
            let userdata = shared.as_ptr() as *const c_void;
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
                epoch: Instant::now(),
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
                autoscroll,
                search: None,
                needle: String::new(),
                cols,
                rows,
                colors,
                seen: HashMap::new(),
                seen_with: (0, Rgb(0, 0, 0), Rgb(0, 0, 0)),
            };
            vt.apply_colors();
            Ok(vt)
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
        let mut palette = [GhosttyColorRgb::default(); 256];
        unsafe {
            ghostty_color_palette_default(palette.as_mut_ptr());
            for (slot, rgb) in palette.iter_mut().zip(self.colors.palette) {
                *slot = c(rgb);
            }
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
                palette.as_ptr() as *const c_void,
            );
        }
    }

    pub fn write(&mut self, bytes: &[u8]) {
        unsafe { ghostty_terminal_vt_write(self.term, bytes.as_ptr(), bytes.len()) };
    }

    pub fn take_replies(&mut self) -> Vec<u8> {
        unsafe { std::mem::take(&mut (*self.shared.as_ptr()).replies) }
    }

    pub fn take_notification(&mut self) -> Option<TermNotification> {
        unsafe { (*self.shared.as_ptr()).notifications.pop_front() }
    }

    /// Whether a synchronized-output hold is still within its deadline.
    pub fn is_held(&self) -> bool {
        unsafe { (*self.shared.as_ptr()).held_since }
            .is_some_and(|since| since.elapsed() < MAX_HOLD)
    }

    /// How long to wait before a held synchronized-output frame must be shown.
    pub fn hold_wait(&self) -> Option<Duration> {
        unsafe { (*self.shared.as_ptr()).held_since }
            .map(|since| MAX_HOLD.saturating_sub(since.elapsed()))
    }

    pub fn cols(&self) -> u16 {
        self.cols
    }

    pub fn rows(&self) -> u16 {
        self.rows
    }

    pub fn cell_pixel_size(&self) -> (f32, f32) {
        let size = unsafe { (*self.shared.as_ptr()).size };
        (size.cell_width.max(1) as f32, size.cell_height.max(1) as f32)
    }

    pub fn resize(&mut self, cols: u16, rows: u16, cell_w: u32, cell_h: u32) {
        if cols == 0 || rows == 0 {
            return;
        }
        self.cols = cols;
        self.rows = rows;
        self.seen.clear();
        unsafe {
            (*self.shared.as_ptr()).size = GhosttySizeReportSize {
                rows,
                columns: cols,
                cell_width: cell_w.max(1),
                cell_height: cell_h.max(1),
            };
            ghostty_terminal_resize(self.term, cols, rows, cell_w.max(1), cell_h.max(1));
        }
    }

    /// The OSC 8 hyperlink on one viewport cell, with the run of cells it
    /// covers on that row. Plain printed URLs are not hyperlinks; the UI finds
    /// those in the snapshot's text. A hyperlink that soft-wrapped is reported
    /// one row at a time: the URI is the whole link, the run is this row's
    /// part of it.
    pub fn hyperlink_at(&self, col: u16, row: u16) -> Option<Hyperlink> {
        if row >= self.rows || col >= self.cols {
            return None;
        }
        let mut target = Vec::new();
        if !self.hyperlink_uri_bytes(col, row, &mut target) {
            return None;
        }
        // The program wrote this URI; control characters have no business
        // reaching the platform's opener.
        let uri: String = String::from_utf8_lossy(&target)
            .chars()
            .filter(|ch| !ch.is_control())
            .collect();
        if uri.is_empty() {
            return None;
        }
        let mut scratch = Vec::with_capacity(target.len());
        let mut from = col;
        while from > 0 && self.hyperlink_uri_bytes(from - 1, row, &mut scratch) && scratch == target
        {
            from -= 1;
        }
        let mut to = col.saturating_add(1);
        while to < self.cols && self.hyperlink_uri_bytes(to, row, &mut scratch) && scratch == target
        {
            to += 1;
        }
        Some(Hyperlink { row, from, to, uri })
    }

    /// Raw OSC 8 URI bytes on one cell of the grid, reusing `buf`.
    fn hyperlink_uri_bytes(&self, col: u16, row: u16, buf: &mut Vec<u8>) -> bool {
        buf.clear();
        unsafe {
            let mut grid = GhosttyGridRef::default();
            let mut point = GhosttyPoint {
                tag: GHOSTTY_POINT_TAG_VIEWPORT,
                ..Default::default()
            };
            point.value.coordinate = GhosttyPointCoordinate {
                x: col,
                y: u32::from(row),
            };
            if ghostty_terminal_grid_ref(self.term, point, &mut grid) != GHOSTTY_SUCCESS {
                return false;
            }
            let mut len: usize = 0;
            // A NULL buffer asks for the size (GHOSTTY_OUT_OF_SPACE) or reports
            // an empty cell (GHOSTTY_SUCCESS with zero bytes).
            ghostty_grid_ref_hyperlink_uri(&grid, std::ptr::null_mut(), 0, &mut len);
            if len == 0 || len > MAX_HYPERLINK_URI_BYTES {
                return false;
            }
            buf.resize(len, 0);
            if ghostty_grid_ref_hyperlink_uri(&grid, buf.as_mut_ptr(), buf.len(), &mut len)
                != GHOSTTY_SUCCESS
            {
                buf.clear();
                return false;
            }
            buf.truncate(len);
            !buf.is_empty()
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
    #[cfg(test)]
    pub fn scroll_to_row(&mut self, row: usize) {
        let mut scroll = GhosttyTerminalScrollViewport {
            tag: GHOSTTY_TERMINAL_SCROLL_VIEWPORT_ROW,
            ..Default::default()
        };
        scroll.value.row = row;
        unsafe { ghostty_terminal_scroll_viewport(self.term, scroll) };
    }
}

impl Drop for Vt {
    fn drop(&mut self) {
        unsafe {
            ghostty_selection_gesture_event_free(self.press);
            ghostty_selection_gesture_event_free(self.drag);
            ghostty_selection_gesture_event_free(self.release);
            ghostty_selection_gesture_event_free(self.autoscroll);
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
            let shared = Box::from_raw(self.shared.as_ptr());
            ghostty_render_state_free(shared.state);
        }
    }
}
