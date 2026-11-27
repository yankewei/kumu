use std::sync::Arc;

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
    /// The cell carries an OSC 8 hyperlink URI.
    pub hyperlink: bool,
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
    /// Default foreground, for text the UI adds on top of the cells (the IME
    /// preview where a hidden cursor leaves no color behind).
    pub fg: Rgb,
    pub cells: Vec<Arc<Vec<Cell>>>,
    /// Whether each row continues on the next one (soft wrap), so a URL split
    /// across rows can be read as one line.
    pub wrap: Vec<bool>,
    pub cursor: Option<Cursor>,
    /// Cursor cell even when the program hides it — where the IME preview is
    /// painted and its candidate window anchors.
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

/// An OSC 8 hyperlink the program wrote, as one row's run of cells.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hyperlink {
    pub row: u16,
    /// First cell of the run (inclusive).
    pub from: u16,
    /// Cell after the run (exclusive).
    pub to: u16,
    pub uri: String,
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
        let colors = TermColors::default();
        Self::blank_with(command, cwd, colors.fg, colors.bg)
    }

    pub fn blank_with(command: &str, cwd: &str, fg: Rgb, bg: Rgb) -> Self {
        Self {
            rows: 24,
            bg,
            fg,
            cells: Vec::new(),
            wrap: Vec::new(),
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

    /// The visible screen as plain text, one line per row.
    #[cfg(test)]
    pub fn plain_text(&self) -> String {
        self.cells
            .iter()
            .map(|row| row_text(row))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// One row as plain text: cell text concatenated, preserving interior blank
/// cells as spaces while skipping wide-character spacer tails.
#[cfg(test)]
fn row_text(row: &[Cell]) -> String {
    let mut text = String::new();
    let mut prev_wide = false;
    for cell in row {
        if std::mem::take(&mut prev_wide) && cell.text.is_empty() {
            continue;
        }
        if cell.text.is_empty() {
            text.push(' ');
        } else {
            text.push_str(&cell.text);
        }
        prev_wide = cell.wide;
    }
    text.trim_end().to_string()
}

pub(super) fn blank_cell(fg: Rgb) -> Cell {
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
        hyperlink: false,
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

/// High-level state reported via the OSC 7501 Program Status Protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProgramState {
    Idle,
    Working,
    Done,
    Blocked,
    Error,
    Clear,
}

/// What a blocked program needs from the user (`kind=` in OSC 7501).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProgramBlockKind {
    Permission,
    Question,
    Auth,
}

/// Aggregated OSC 7501 status emitted by a terminal after applying a report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TermProgramStatus {
    pub state: ProgramState,
    pub kind: Option<ProgramBlockKind>,
    pub app: String,
    pub title: String,
    pub progress: Option<u8>,
    pub message: String,
}
