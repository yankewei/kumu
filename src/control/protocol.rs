use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::Sender;

use serde::{Deserialize, Serialize};

use crate::db::{TodoItem, TodoStatus};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ScrollDirection {
    Up,
    Down,
}

impl ScrollDirection {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Down => "down",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "op", rename_all = "camelCase")]
pub enum Request {
    /// Every pane and the facts the app knows about it.
    Panes,
    /// One pane's screen, preceded by some scrollback, as plain text.
    Read {
        /// A pane id, a direction (`left`/`right`/`up`/`down`), or `focused`.
        pane: PaneTarget,
        /// The caller's own pane, which a direction is relative to.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<u64>,
        /// Scrollback rows above the screen.
        #[serde(default)]
        history: usize,
    },
    /// Scroll a pane up (older content) or down (newer content).
    Scroll {
        pane: PaneTarget,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<u64>,
        direction: ScrollDirection,
        /// Number of lines / wheel ticks. Defaults to half the pane height.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lines: Option<u16>,
        /// Optional 0-based column for mouse-wheel TUIs (defaults to `cols / 3`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        col: Option<u16>,
        /// Optional 0-based row for mouse-wheel TUIs (defaults to `rows / 2`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        row: Option<u16>,
    },
    /// Split a pane and start an agent in the new one.
    Split {
        /// `right` or `down`: Kumu appends a pane after its target, so there
        /// is no left or up.
        direction: String,
        /// A registry id (`codex`) or display name (`Codex`).
        agent: String,
        /// The pane the split grows from; the focused pane when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<u64>,
        /// Sent once the agent has drawn its prompt, so the new pane starts
        /// working instead of waiting for someone to type.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt: Option<String>,
    },
    /// Type text into an agent pane.
    Send {
        pane: PaneTarget,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<u64>,
        text: String,
        /// Press Enter after the text. A program left without it keeps the
        /// text on its input line.
        #[serde(default = "submit_by_default")]
        enter: bool,
    },
    /// Close a pane. Refuses to close the caller's own pane unless `allow_self`.
    Close {
        pane: PaneTarget,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<u64>,
        #[serde(default)]
        allow_self: bool,
    },
    /// Wait until an agent pane enters a terminal/attention state (`waiting`,
    /// `completed`, `failed`) or exits.
    Wait {
        pane: PaneTarget,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<u64>,
        #[serde(default = "default_wait_timeout")]
        timeout_secs: u64,
        #[serde(default)]
        next: bool,
    },
    /// List TODO items on the caller's repository board.
    TodoList {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<TodoStatus>,
        #[serde(default)]
        mine: bool,
    },
    /// Show one TODO item in detail.
    TodoShow {
        id: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<u64>,
    },
    /// Create a TODO item on the caller's repository board.
    TodoAdd {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<u64>,
        title: String,
        #[serde(default)]
        description: String,
        #[serde(default = "todo_status_default")]
        status: TodoStatus,
        #[serde(default)]
        bind: bool,
    },
    /// Update one TODO item's fields, status, review mark, or pane link.
    TodoUpdate {
        id: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<TodoStatus>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        awaiting_review: Option<bool>,
        #[serde(default)]
        bind: bool,
        #[serde(default)]
        unbind: bool,
    },
    /// Remove one TODO item.
    TodoRemove {
        id: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<u64>,
    },
    /// Import an external GitHub/GitLab/Jira issue onto the caller's repository board.
    TodoImport {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<u64>,
        input: String,
        #[serde(default = "todo_status_default")]
        status: TodoStatus,
        #[serde(default)]
        bind: bool,
    },
}

fn submit_by_default() -> bool {
    true
}

pub(super) const DEFAULT_WAIT_TIMEOUT_SECS: u64 = 300;
pub(super) const MAX_WAIT_TIMEOUT_SECS: u64 = 3600;

fn default_wait_timeout() -> u64 {
    DEFAULT_WAIT_TIMEOUT_SECS
}

fn todo_status_default() -> TodoStatus {
    TodoStatus::Todo
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum PaneTarget {
    Id(u64),
    Name(String),
}

/// One request line: the request itself, plus the pid of the Kumu the caller
/// belongs to. A pane sends its own app's pid so a socket served by another
/// Kumu instance refuses the request instead of acting on the wrong window.
/// A caller outside any pane (a plain terminal) sends none and is accepted.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestLine {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(flatten)]
    pub request: Request,
}

/// What a pane is, without its screen. This is the row both `panes` and a
/// read's header are built from.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PaneSummary {
    pub id: u64,
    pub project: i64,
    pub project_name: String,
    /// The program's own title, else the program's name.
    pub title: String,
    /// The program this pane started.
    pub command: String,
    pub cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// The window's focused pane.
    pub focused: bool,
    /// In the project the window is showing.
    pub selected: bool,
    pub cols: u16,
    pub rows: u16,
    pub exited: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PaneContent {
    pub pane: PaneSummary,
    pub text: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SplitResult {
    /// The pane that was added.
    pub pane: PaneSummary,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SendResult {
    pub pane: PaneSummary,
    /// Whether Enter follows the text.
    pub submitted: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CloseResult {
    pub pane: u64,
    pub closed: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WaitResult {
    pub pane: PaneSummary,
    /// Resulting agent state (`waiting`, `completed`, `failed`, or `exited`).
    pub state: String,
    pub elapsed_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScrollResult {
    pub pane: PaneSummary,
    pub direction: ScrollDirection,
    /// Requested (or default half-screen) lines / wheel ticks.
    pub lines: u16,
    /// Actual primary-screen viewport offset delta (`0` = already at limit),
    /// or `None` when forwarded to a TUI (`mouse_tracking` / `alt_screen`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub moved: Option<i32>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TodoImportResult {
    pub todo: TodoItem,
    pub created: bool,
}

#[derive(Clone, Debug)]
pub struct ControlError {
    pub kind: &'static str,
    pub message: String,
}

impl ControlError {
    pub fn new(kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

/// A request the server forwards to the home window. The reply channel is a
/// plain `std` sender: the UI thread answers it, sometimes from a background
/// task, and the socket thread is already blocking on it.
pub enum Command {
    Panes {
        reply: Sender<Result<Vec<PaneSummary>, ControlError>>,
    },
    Read {
        target: PaneTarget,
        from: Option<u64>,
        history: usize,
        reply: Sender<Result<PaneContent, ControlError>>,
    },
    Scroll {
        target: PaneTarget,
        from: Option<u64>,
        direction: ScrollDirection,
        lines: Option<u16>,
        col: Option<u16>,
        row: Option<u16>,
        reply: Sender<Result<ScrollResult, ControlError>>,
    },
    Split {
        direction: String,
        agent: String,
        from: Option<u64>,
        prompt: Option<String>,
        cancelled: Arc<AtomicBool>,
        reply: Sender<Result<SplitResult, ControlError>>,
    },
    Send {
        target: PaneTarget,
        from: Option<u64>,
        text: String,
        enter: bool,
        reply: Sender<Result<SendResult, ControlError>>,
    },
    Close {
        target: PaneTarget,
        from: Option<u64>,
        allow_self: bool,
        reply: Sender<Result<CloseResult, ControlError>>,
    },
    Wait {
        target: PaneTarget,
        from: Option<u64>,
        timeout_secs: u64,
        next: bool,
        cancelled: Arc<AtomicBool>,
        reply: Sender<Result<WaitResult, ControlError>>,
    },
    TodoList {
        from: Option<u64>,
        status: Option<TodoStatus>,
        mine: bool,
        reply: Sender<Result<Vec<TodoItem>, ControlError>>,
    },
    TodoShow {
        id: i64,
        from: Option<u64>,
        reply: Sender<Result<TodoItem, ControlError>>,
    },
    TodoAdd {
        from: Option<u64>,
        title: String,
        description: String,
        status: TodoStatus,
        bind: bool,
        reply: Sender<Result<TodoItem, ControlError>>,
    },
    TodoUpdate {
        id: i64,
        from: Option<u64>,
        title: Option<String>,
        description: Option<String>,
        status: Option<TodoStatus>,
        awaiting_review: Option<bool>,
        bind: bool,
        unbind: bool,
        reply: Sender<Result<TodoItem, ControlError>>,
    },
    TodoRemove {
        id: i64,
        from: Option<u64>,
        reply: Sender<Result<bool, ControlError>>,
    },
    TodoImport {
        from: Option<u64>,
        input: String,
        status: TodoStatus,
        bind: bool,
        reply: Sender<Result<TodoImportResult, ControlError>>,
    },
}
