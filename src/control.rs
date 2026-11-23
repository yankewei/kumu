//! Local control channel: what a program inside a pane may ask the app.
//!
//! Everything a pane shows lives in this process, so a program in one pane (an
//! agent, a script) reaches that state over a Unix socket: one JSON request
//! line in, one JSON reply line out. The socket is owner-only under `~/.kumu`.
//! It lists the panes, reads one pane's text, splits a pane into a new agent,
//! and types into an agent pane — a shell pane never takes input, because text
//! there would run as a command.
//!
//! The same file holds both ends: the CLI a pane's agent runs and the server
//! the home window starts.

use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use futures::channel::mpsc::UnboundedSender;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::db::{TodoItem, TodoStatus};

/// How long the server waits for the UI to answer one request. A split that
/// waits for the agent to draw its prompt is the slow one.
const REPLY_TIMEOUT: Duration = Duration::from_secs(15);
/// How often the accept loop looks at its stop flag.
const POLL: Duration = Duration::from_millis(25);
/// Upper bound on one JSON request line accepted by the control socket.
const MAX_REQUEST_BYTES: u64 = 256 * 1024;
/// Upper bound on one JSON reply line read by the CLI client.
const MAX_RESPONSE_BYTES: u64 = 16 * 1024 * 1024;
/// Maximum concurrent control-socket worker threads.
const MAX_CONCURRENT_CLIENTS: usize = 32;
/// The most scrollback rows one read may ask for. Reading history pages the
/// viewport, so the bound is work, not storage.
pub const MAX_HISTORY: usize = 2_000;
/// Maximum lines / wheel ticks accepted in a single `pane scroll` request.
pub const MAX_SCROLL_LINES: u16 = 500;
/// The pid of the Kumu a pane's process belongs to. Kumu exports it to every
/// pane, so a client can tell its own app apart from another instance that
/// happens to serve the socket.
pub const PID_ENV: &str = "KUMU_APP_PID";

unsafe extern "C" {
    fn geteuid() -> u32;
    fn flock(fd: i32, operation: i32) -> i32;
    #[cfg(target_os = "macos")]
    fn getpeereid(fd: i32, euid: *mut u32, egid: *mut u32) -> i32;
}

const LOCK_EX: i32 = 2;
const LOCK_NB: i32 = 4;

/// Where the socket lives. `KUMU_CONTROL_SOCKET` lets a test or a second
/// instance pick its own; the default sits beside the database.
pub fn socket_path() -> PathBuf {
    if let Some(path) = std::env::var_os("KUMU_CONTROL_SOCKET").filter(|v| !v.is_empty()) {
        return PathBuf::from(path);
    }
    let db = crate::db::db_path();
    if db.as_os_str().is_empty() {
        return PathBuf::new();
    }
    db.with_file_name("control.sock")
}

// ---------------------------------------------------------------- protocol

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
}

fn submit_by_default() -> bool {
    true
}

const DEFAULT_WAIT_TIMEOUT_SECS: u64 = 300;
const MAX_WAIT_TIMEOUT_SECS: u64 = 3600;

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
}

// ------------------------------------------------------------------ server

/// Keeps the control socket alive. Dropping it stops the accept loop and
/// removes the socket, so a closed window leaves nothing for a client to
/// connect to.
pub struct ControlHandle {
    path: PathBuf,
    inode: Option<(u64, u64)>,
    _lock: std::fs::File,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for ControlHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        // Only remove the socket file if it is still the exact inode we bound.
        if let Some((dev, ino)) = self.inode
            && let Ok(meta) = std::fs::symlink_metadata(&self.path)
            && meta.file_type().is_socket()
            && (meta.dev(), meta.ino()) == (dev, ino)
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Bind the socket and serve it from a thread of its own. Returns `None` when
/// another Kumu already owns the path, or when the directory cannot be made
/// private: a socket other users could open must not exist.
pub fn start(commands: UnboundedSender<Command>) -> Option<ControlHandle> {
    start_at(socket_path(), commands)
}

fn start_at(path: PathBuf, commands: UnboundedSender<Command>) -> Option<ControlHandle> {
    if path.as_os_str().is_empty() {
        return None;
    }
    let dir = path.parent()?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .ok()?;
    let dir_meta = std::fs::symlink_metadata(dir).ok()?;
    let my_uid = unsafe { geteuid() };
    if dir_meta.file_type().is_symlink() || !dir_meta.is_dir() || dir_meta.uid() != my_uid {
        eprintln!(
            "[control] refusing unsafe socket directory {}",
            dir.display()
        );
        return None;
    }
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).ok()?;

    // Serialize socket ownership across concurrent Kumu launches with an
    // exclusive non-blocking flock on a sibling lockfile.
    let lock_path = path.with_extension("lock");
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(&lock_path)
        .ok()?;
    if unsafe { flock(lock_file.as_raw_fd(), LOCK_EX | LOCK_NB) } != 0 {
        eprintln!("[control] {} is served by another Kumu", path.display());
        return None;
    }

    // A socket file left by a crashed run is dead; never unlink a non-socket.
    if let Ok(meta) = std::fs::symlink_metadata(&path) {
        if !meta.file_type().is_socket() {
            eprintln!(
                "[control] refusing to replace non-socket {}",
                path.display()
            );
            return None;
        }
        if UnixStream::connect(&path).is_ok() {
            eprintln!("[control] {} is served by another Kumu", path.display());
            return None;
        }
        let _ = std::fs::remove_file(&path);
    }

    let listener = match UnixListener::bind(&path) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("[control] cannot bind {}: {error}", path.display());
            return None;
        }
    };
    if let Err(error) = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)) {
        eprintln!("[control] cannot chmod {}: {error}", path.display());
        let _ = std::fs::remove_file(&path);
        return None;
    }
    let inode = std::fs::symlink_metadata(&path)
        .ok()
        .map(|meta| (meta.dev(), meta.ino()));
    if let Err(error) = listener.set_nonblocking(true) {
        eprintln!("[control] cannot watch {}: {error}", path.display());
        let _ = std::fs::remove_file(&path);
        return None;
    }
    let stop = Arc::new(AtomicBool::new(false));
    let accepting = Arc::clone(&stop);
    let thread = match std::thread::Builder::new()
        .name("kumu-control".into())
        .spawn(move || accept_loop(listener, commands, accepting))
    {
        Ok(thread) => thread,
        Err(error) => {
            eprintln!("[control] cannot spawn control thread: {error}");
            let _ = std::fs::remove_file(&path);
            return None;
        }
    };
    Some(ControlHandle {
        path,
        inode,
        _lock: lock_file,
        stop,
        thread: Some(thread),
    })
}

struct ActiveClientGuard(Arc<AtomicUsize>);

impl Drop for ActiveClientGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn accept_loop(listener: UnixListener, commands: UnboundedSender<Command>, stop: Arc<AtomicBool>) {
    let active = Arc::new(AtomicUsize::new(0));
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((mut stream, _)) => {
                if active.fetch_add(1, Ordering::AcqRel) >= MAX_CONCURRENT_CLIENTS {
                    active.fetch_sub(1, Ordering::AcqRel);
                    let _ = stream.set_nonblocking(false);
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
                    let body = failure(&ControlError::new(
                        "busy",
                        "too many concurrent control requests",
                    ));
                    let _ = stream.write_all(format!("{body}\n").as_bytes());
                    continue;
                }
                let guard = ActiveClientGuard(Arc::clone(&active));
                let commands = commands.clone();
                // Use Builder::spawn so thread-creation failure never panics
                // the accept loop.
                if std::thread::Builder::new()
                    .name("kumu-control-client".into())
                    .spawn(move || {
                        let _guard = guard;
                        serve(stream, &commands);
                    })
                    .is_err()
                {
                    let _ = active.fetch_sub(1, Ordering::AcqRel);
                }
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => std::thread::sleep(POLL),
            Err(_) => std::thread::sleep(POLL),
        }
    }
}

fn peer_is_same_user(stream: &UnixStream) -> bool {
    #[cfg(target_os = "macos")]
    {
        let mut euid: u32 = u32::MAX;
        let mut egid: u32 = u32::MAX;
        let ok = unsafe { getpeereid(stream.as_raw_fd(), &mut euid, &mut egid) == 0 };
        ok && euid == unsafe { geteuid() }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = stream;
        true
    }
}

fn read_bounded_line(
    stream: &UnixStream,
    max_bytes: u64,
    deadline: Instant,
) -> Result<String, &'static str> {
    let mut reader = BufReader::new(stream.take(max_bytes + 1));
    let mut bytes = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("request read timed out");
        }
        let _ = stream.set_read_timeout(Some(remaining));
        let mut chunk = Vec::new();
        match reader.read_until(b'\n', &mut chunk) {
            Ok(0) => break,
            Ok(_) => {
                bytes.extend_from_slice(&chunk);
                if bytes.len() as u64 > max_bytes {
                    return Err("request exceeds maximum size");
                }
                if bytes.ends_with(b"\n") {
                    break;
                }
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(_) => return Err("expected one JSON request line"),
        }
    }
    if bytes.is_empty() || !bytes.ends_with(b"\n") {
        return Err("expected one newline-terminated JSON request line");
    }
    String::from_utf8(bytes).map_err(|_| "request must be valid UTF-8")
}

/// One connection, one request, one reply.
fn serve(stream: UnixStream, commands: &UnboundedSender<Command>) {
    let mut stream = stream;
    // The listener accepts in nonblocking mode, and macOS hands that flag to
    // the connection: left as is, a request that lands a moment after the
    // connection reads as WouldBlock and looks like an empty one.
    if stream.set_nonblocking(false).is_err() {
        return;
    }
    if !peer_is_same_user(&stream) {
        return;
    }
    let write = stream.set_write_timeout(Some(REPLY_TIMEOUT));
    if write.is_err() {
        return;
    }
    let deadline = Instant::now() + REPLY_TIMEOUT;
    let reply = match read_bounded_line(&stream, MAX_REQUEST_BYTES, deadline) {
        Ok(line) => match serde_json::from_str::<RequestLine>(line.trim()) {
            Ok(line) => match line.pid {
                Some(pid) if pid != std::process::id() => Err(ControlError::new(
                    "wrongInstance",
                    format!(
                        "this socket belongs to another Kumu (pid {}); the caller belongs to pid {pid}",
                        std::process::id()
                    ),
                )),
                _ => ask(commands, line.request),
            },
            Err(error) => Err(ControlError::new("badRequest", error.to_string())),
        },
        Err(msg) => Err(ControlError::new("badRequest", msg)),
    };
    let body = match reply {
        Ok(value) => json!({ "ok": true, "pid": std::process::id(), "result": value }),
        Err(error) => failure(&error),
    };
    let _ = stream.write_all(format!("{body}\n").as_bytes());
    let _ = stream.flush();
}

fn failure(error: &ControlError) -> Value {
    json!({
        "ok": false,
        "pid": std::process::id(),
        "error": error.kind,
        "message": error.message,
    })
}

fn ask(commands: &UnboundedSender<Command>, request: Request) -> Result<Value, ControlError> {
    match request {
        Request::Panes => {
            let (reply, answer) = mpsc::channel();
            commands
                .unbounded_send(Command::Panes { reply })
                .map_err(|_| unavailable())?;
            match answer.recv_timeout(REPLY_TIMEOUT) {
                Ok(Ok(panes)) => Ok(json!({ "panes": panes })),
                Ok(Err(error)) => Err(error),
                Err(_) => Err(ControlError::new("timeout", "kumu did not answer")),
            }
        }
        Request::Read {
            pane,
            from,
            history,
        } => exchange(commands, |reply| Command::Read {
            target: pane,
            from,
            history: history.min(MAX_HISTORY),
            reply,
        }),
        Request::Scroll {
            pane,
            from,
            direction,
            lines,
            col,
            row,
        } => {
            if let Some(n) = lines
                && !(1..=MAX_SCROLL_LINES).contains(&n)
            {
                return Err(ControlError::new(
                    "badRequest",
                    format!("lines must be between 1 and {MAX_SCROLL_LINES}"),
                ));
            }
            exchange(commands, |reply| Command::Scroll {
                target: pane,
                from,
                direction,
                lines,
                col,
                row,
                reply,
            })
        }
        Request::Split {
            direction,
            agent,
            from,
            prompt,
        } => {
            let cancelled = Arc::new(AtomicBool::new(false));
            let cancel_flag = Arc::clone(&cancelled);
            let result = exchange(commands, |reply| Command::Split {
                direction,
                agent,
                from,
                prompt,
                cancelled: cancel_flag,
                reply,
            });
            if result.is_err() {
                cancelled.store(true, Ordering::Release);
            }
            result
        }
        Request::Send {
            pane,
            from,
            text,
            enter,
        } => exchange(commands, |reply| Command::Send {
            target: pane,
            from,
            text,
            enter,
            reply,
        }),
        Request::Close {
            pane,
            from,
            allow_self,
        } => exchange(commands, |reply| Command::Close {
            target: pane,
            from,
            allow_self,
            reply,
        }),
        Request::Wait {
            pane,
            from,
            timeout_secs,
            next,
        } => {
            let clamped = timeout_secs.clamp(1, MAX_WAIT_TIMEOUT_SECS);
            let cancelled = Arc::new(AtomicBool::new(false));
            let cancel_flag = Arc::clone(&cancelled);
            let result =
                exchange_with_timeout(commands, Duration::from_secs(clamped + 5), |reply| {
                    Command::Wait {
                        target: pane,
                        from,
                        timeout_secs: clamped,
                        next,
                        cancelled: cancel_flag,
                        reply,
                    }
                });
            if result.is_err() {
                cancelled.store(true, Ordering::Release);
            }
            result
        }
        Request::TodoList { from, status, mine } => {
            let (reply, answer) = mpsc::channel();
            commands
                .unbounded_send(Command::TodoList {
                    from,
                    status,
                    mine,
                    reply,
                })
                .map_err(|_| unavailable())?;
            match answer.recv_timeout(REPLY_TIMEOUT) {
                Ok(Ok(todos)) => Ok(json!({ "todos": todos })),
                Ok(Err(error)) => Err(error),
                Err(_) => Err(ControlError::new("timeout", "kumu did not answer")),
            }
        }
        Request::TodoShow { id, from } => {
            exchange(commands, |reply| Command::TodoShow { id, from, reply })
        }
        Request::TodoAdd {
            from,
            title,
            description,
            status,
            bind,
        } => exchange(commands, |reply| Command::TodoAdd {
            from,
            title,
            description,
            status,
            bind,
            reply,
        }),
        Request::TodoUpdate {
            id,
            from,
            title,
            description,
            status,
            awaiting_review,
            bind,
            unbind,
        } => exchange(commands, |reply| Command::TodoUpdate {
            id,
            from,
            title,
            description,
            status,
            awaiting_review,
            bind,
            unbind,
            reply,
        }),
        Request::TodoRemove { id, from } => {
            let (reply, answer) = mpsc::channel();
            commands
                .unbounded_send(Command::TodoRemove { id, from, reply })
                .map_err(|_| unavailable())?;
            match answer.recv_timeout(REPLY_TIMEOUT) {
                Ok(Ok(removed)) => Ok(json!({ "id": id, "removed": removed })),
                Ok(Err(error)) => Err(error),
                Err(_) => Err(ControlError::new("timeout", "kumu did not answer")),
            }
        }
    }
}

/// One round trip through the UI: send the command, wait for its answer, and
/// turn the answer into the reply body.
fn exchange<T: Serialize>(
    commands: &UnboundedSender<Command>,
    command: impl FnOnce(Sender<Result<T, ControlError>>) -> Command,
) -> Result<Value, ControlError> {
    exchange_with_timeout(commands, REPLY_TIMEOUT, command)
}

fn exchange_with_timeout<T: Serialize>(
    commands: &UnboundedSender<Command>,
    timeout: Duration,
    command: impl FnOnce(Sender<Result<T, ControlError>>) -> Command,
) -> Result<Value, ControlError> {
    let (reply, answer) = mpsc::channel();
    commands
        .unbounded_send(command(reply))
        .map_err(|_| unavailable())?;
    match answer.recv_timeout(timeout) {
        Ok(Ok(value)) => serde_json::to_value(value)
            .map_err(|error| ControlError::new("internal", error.to_string())),
        Ok(Err(error)) => Err(error),
        Err(_) => Err(ControlError::new("timeout", "kumu did not answer")),
    }
}

fn unavailable() -> ControlError {
    ControlError::new("unavailable", "kumu is not running")
}

// --------------------------------------------------------------------- CLI

/// The command line a pane's agent runs:
///
/// ```text
/// kumu panes
/// kumu pane read right --history 200 --tail 40
/// kumu pane split right --agent codex --prompt "review the diff"
/// kumu pane send right "review the diff"
/// ```
///
/// `None` means the arguments are not this CLI's, so `main` continues into the
/// app. Anything that matches here exits with the returned code.
pub fn run_cli() -> Option<i32> {
    let mut args = std::env::args().skip(1);
    let first = args.next()?;
    let rest: Vec<String> = args.collect();
    match first.as_str() {
        "panes" => Some(panes_cli(&rest)),
        "pane" => Some(pane_cli(&rest)),
        "todo" => Some(todo_cli(&rest)),
        _ => None,
    }
}

const USAGE: &str = "\
usage:
  kumu panes [--json]
  kumu pane read [TARGET] [--history N] [--tail N] [--json]
  kumu pane scroll up|down [TARGET] [--lines N] [--col C] [--row R] [--json]
  kumu pane split right|down --agent <id> [--prompt TEXT] [--json]
  kumu pane send <TARGET> <TEXT...> [--no-enter] [--json]
  kumu pane wait <TARGET> [--timeout SECS] [--next] [--json]
  kumu pane close <TARGET> [--self] [--json]
  kumu todo list [--status todo|in_progress|done] [--mine] [--json]
  kumu todo show <ID> [--json]
  kumu todo add <TITLE...> [--desc TEXT] [--status todo|in_progress|done] [--bind] [--json]
  kumu todo update <ID> [--title TEXT] [--desc TEXT] [--status todo|in_progress|done] [--review|--no-review] [--bind|--unbind] [--json]
  kumu todo review <ID> [--json]
  kumu todo done <ID> [--json]
  kumu todo remove <ID> [--json]

TARGET is a pane id, left/right/up/down (relative to $KUMU_PANE), or focused
(the default). --history N reads N scrollback rows; --tail N keeps the last N
lines of the answer. A split starts an agent in the new pane; its --prompt is
sent once that agent has drawn its own prompt. In send, -- makes everything
after it text, so it may start with a dash. wait blocks until the target agent
enters waiting/completed/failed or exits (--next waits for the next transition).";

fn panes_cli(args: &[String]) -> i32 {
    let (json, rest) = match flags(args) {
        Ok(parsed) => parsed,
        Err(code) => return code,
    };
    if !rest.is_empty() {
        eprintln!("{USAGE}");
        return 2;
    }
    let value = match call(&Request::Panes) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("kumu: {error}");
            return 1;
        }
    };
    if json {
        let Some(panes) = value.get("panes").filter(|v| v.is_array()) else {
            eprintln!("kumu: unexpected reply: missing panes array");
            return 1;
        };
        println!("{panes}");
        return 0;
    }
    let panes: Vec<PaneSummary> = match serde_json::from_value(value["panes"].clone()) {
        Ok(panes) => panes,
        Err(error) => {
            eprintln!("kumu: unexpected reply: {error}");
            return 1;
        }
    };
    if panes.is_empty() {
        println!("no panes");
        return 0;
    }
    for pane in panes {
        println!("{}", summary_line(&pane));
    }
    0
}

fn pane_cli(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("read") => pane_read(&args[1..]),
        Some("scroll") => pane_scroll(&args[1..]),
        Some("split") => pane_split(&args[1..]),
        Some("send") => pane_send(&args[1..]),
        Some("wait") => pane_wait(&args[1..]),
        Some("close") => pane_close(&args[1..]),
        Some("-h" | "--help" | "help") => {
            println!("{USAGE}");
            0
        }
        _ => {
            eprintln!("{USAGE}");
            2
        }
    }
}

fn pane_wait(args: &[String]) -> i32 {
    let mut target = None;
    let mut timeout_secs = DEFAULT_WAIT_TIMEOUT_SECS;
    let mut next = false;
    let mut json = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--json" => json = true,
            "--next" => next = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return 0;
            }
            "--timeout" | "-t" => match iter.next().and_then(|n| n.parse::<u64>().ok()) {
                Some(secs) if (1..=MAX_WAIT_TIMEOUT_SECS).contains(&secs) => timeout_secs = secs,
                _ => {
                    return usage_error(&format!(
                        "--timeout needs seconds between 1 and {MAX_WAIT_TIMEOUT_SECS}"
                    ));
                }
            },
            other if other.starts_with('-') => {
                return usage_error(&format!("unknown option {other}"));
            }
            other if target.is_none() => target = Some(other.to_string()),
            _ => return usage_error("one target at a time"),
        }
    }
    let Some(target) = target else {
        return usage_error("name the target pane");
    };
    let value = match call(&Request::Wait {
        pane: pane_target(Some(target)),
        from: caller_pane(),
        timeout_secs,
        next,
    }) {
        Ok(value) => value,
        Err(error) => return runtime_error(&error),
    };
    let result: WaitResult = match serde_json::from_value(value) {
        Ok(result) => result,
        Err(error) => return runtime_error(&format!("unexpected reply: {error}")),
    };
    if json {
        return print_json(&result);
    }
    println!(
        "{} · state {} · {:.1}s",
        summary_line(&result.pane),
        result.state,
        result.elapsed_ms as f64 / 1000.0
    );
    0
}

fn pane_close(args: &[String]) -> i32 {
    let mut target = None;
    let mut allow_self = false;
    let mut json = false;
    for arg in args {
        match arg.as_str() {
            "--json" => json = true,
            "--self" => allow_self = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return 0;
            }
            other if other.starts_with('-') => {
                return usage_error(&format!("unknown option {other}"));
            }
            other if target.is_none() => target = Some(other.to_string()),
            _ => return usage_error("one target at a time"),
        }
    }
    let Some(target) = target else {
        return usage_error("name the target pane");
    };
    let value = match call(&Request::Close {
        pane: pane_target(Some(target)),
        from: caller_pane(),
        allow_self,
    }) {
        Ok(value) => value,
        Err(error) => return runtime_error(&error),
    };
    let result: CloseResult = match serde_json::from_value(value) {
        Ok(result) => result,
        Err(error) => return runtime_error(&format!("unexpected reply: {error}")),
    };
    if json {
        return print_json(&result);
    }
    println!("closed pane {}", result.pane);
    0
}

fn pane_read(args: &[String]) -> i32 {
    let mut target = None;
    let mut history = 0usize;
    let mut tail = None;
    let mut json = false;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--json" => json = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return 0;
            }
            "--history" | "-H" => match args.next().and_then(|n| n.parse().ok()) {
                Some(n) => history = n,
                None => return usage_error("--history needs a number"),
            },
            "--tail" | "-n" => match args.next().and_then(|n| n.parse().ok()) {
                Some(n) => tail = Some(n),
                None => return usage_error("--tail needs a number"),
            },
            other if other.starts_with('-') => {
                return usage_error(&format!("unknown option {other}"));
            }
            other => {
                if target.is_some() {
                    return usage_error("one target at a time");
                }
                target = Some(other.to_string());
            }
        }
    }
    let target = pane_target(target);
    let value = match call(&Request::Read {
        pane: target,
        from: caller_pane(),
        history,
    }) {
        Ok(value) => value,
        Err(error) => return runtime_error(&error),
    };
    let mut content: PaneContent = match serde_json::from_value(value) {
        Ok(content) => content,
        Err(error) => return runtime_error(&format!("unexpected reply: {error}")),
    };
    if let Some(tail) = tail {
        content.text = last_lines(&content.text, tail);
    }
    if json {
        return print_json(&content);
    }
    println!("{}", summary_line(&content.pane));
    println!("---");
    println!("{}", content.text.trim_end());
    0
}

#[derive(Debug, PartialEq, Eq)]
struct ScrollCliArgs {
    direction: ScrollDirection,
    target: Option<String>,
    lines: Option<u16>,
    col: Option<u16>,
    row: Option<u16>,
    json: bool,
}

enum ScrollParse {
    Help,
    Parsed(ScrollCliArgs),
    Error(String),
}

fn parse_pane_scroll(args: &[String]) -> ScrollParse {
    let mut direction = None;
    let mut target = None;
    let mut lines = None;
    let mut col = None;
    let mut row = None;
    let mut json = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--json" => json = true,
            "-h" | "--help" => return ScrollParse::Help,
            "--lines" | "-n" => match iter.next().and_then(|n| n.parse::<u16>().ok()) {
                Some(n) if (1..=MAX_SCROLL_LINES).contains(&n) => lines = Some(n),
                _ => {
                    return ScrollParse::Error(format!(
                        "--lines needs a number between 1 and {MAX_SCROLL_LINES}"
                    ));
                }
            },
            "--col" => match iter.next().and_then(|n| n.parse::<u16>().ok()) {
                Some(n) => col = Some(n),
                None => return ScrollParse::Error("--col needs a number".into()),
            },
            "--row" => match iter.next().and_then(|n| n.parse::<u16>().ok()) {
                Some(n) => row = Some(n),
                None => return ScrollParse::Error("--row needs a number".into()),
            },
            other if other.starts_with('-') => {
                return ScrollParse::Error(format!("unknown option {other}"));
            }
            other if direction.is_none() => match other {
                "up" => direction = Some(ScrollDirection::Up),
                "down" => direction = Some(ScrollDirection::Down),
                _ => {
                    return ScrollParse::Error(
                        "declare the direction first: up or down".into(),
                    );
                }
            },
            other if target.is_none() => target = Some(other.to_string()),
            _ => return ScrollParse::Error("one target at a time".into()),
        }
    }
    let Some(direction) = direction else {
        return ScrollParse::Error("declare the direction: up or down".into());
    };
    ScrollParse::Parsed(ScrollCliArgs {
        direction,
        target,
        lines,
        col,
        row,
        json,
    })
}

fn pane_scroll(args: &[String]) -> i32 {
    let parsed = match parse_pane_scroll(args) {
        ScrollParse::Help => {
            println!("{USAGE}");
            return 0;
        }
        ScrollParse::Error(msg) => return usage_error(&msg),
        ScrollParse::Parsed(parsed) => parsed,
    };
    let value = match call(&Request::Scroll {
        pane: pane_target(parsed.target),
        from: caller_pane(),
        direction: parsed.direction,
        lines: parsed.lines,
        col: parsed.col,
        row: parsed.row,
    }) {
        Ok(value) => value,
        Err(error) => return runtime_error(&error),
    };
    let result: ScrollResult = match serde_json::from_value(value) {
        Ok(result) => result,
        Err(error) => return runtime_error(&format!("unexpected reply: {error}")),
    };
    if parsed.json {
        return print_json(&result);
    }
    match result.moved {
        Some(moved) => println!(
            "scrolled pane {} {} {} lines (moved {moved})",
            result.pane.id,
            result.direction.as_str(),
            result.lines
        ),
        None => println!(
            "scrolled pane {} {} {} lines",
            result.pane.id,
            result.direction.as_str(),
            result.lines
        ),
    }
    0
}

fn pane_split(args: &[String]) -> i32 {
    let mut direction = None;
    let mut agent = None;
    let mut prompt: Option<String> = None;
    let mut trailing = Vec::new();
    let mut json = false;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--json" => json = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return 0;
            }
            "--agent" | "-a" => match args.next() {
                Some(value) => agent = Some(value.clone()),
                None => return usage_error("--agent needs an id"),
            },
            "--prompt" | "-p" => match args.next() {
                Some(value) => prompt = Some(value.clone()),
                None => return usage_error("--prompt needs text"),
            },
            other if other.starts_with('-') => {
                return usage_error(&format!("unknown option {other}"));
            }
            other if direction.is_none() => direction = Some(other.to_string()),
            other => trailing.push(other.to_string()),
        }
    }
    let Some(direction) = direction else {
        return usage_error("declare the direction: right or down");
    };
    let Some(agent) = agent else {
        return usage_error("name the agent: --agent codex");
    };
    // Words left over after the direction read as the prompt, so quoting is
    // optional: `split right --agent codex review the diff`.
    if prompt.is_none() && !trailing.is_empty() {
        prompt = Some(trailing.join(" "));
    } else if prompt.is_some() && !trailing.is_empty() {
        return usage_error("the prompt is either --prompt TEXT or trailing words");
    }
    if prompt
        .as_deref()
        .is_some_and(|prompt| prompt.trim().is_empty())
    {
        return usage_error("--prompt needs text");
    }
    let value = match call(&Request::Split {
        direction,
        agent,
        from: caller_pane(),
        prompt,
    }) {
        Ok(value) => value,
        Err(error) => return runtime_error(&error),
    };
    let split: SplitResult = match serde_json::from_value(value) {
        Ok(split) => split,
        Err(error) => return runtime_error(&format!("unexpected reply: {error}")),
    };
    if json {
        return print_json(&split);
    }
    println!("{}", summary_line(&split.pane));
    0
}

fn pane_send(args: &[String]) -> i32 {
    let mut target = None;
    let mut words = Vec::new();
    let mut enter = true;
    let mut json = false;
    // Everything after `--` is text, so it may start with a dash or be a word
    // that looks like a flag.
    let mut text_only = false;
    for arg in args {
        if text_only {
            words.push(arg.clone());
            continue;
        }
        match arg.as_str() {
            "--" if target.is_some() => text_only = true,
            "--json" => json = true,
            "--no-enter" => enter = false,
            "-h" | "--help" if words.is_empty() => {
                println!("{USAGE}");
                return 0;
            }
            other if other.starts_with('-') && words.is_empty() => {
                return usage_error(&format!("unknown option {other}"));
            }
            other if target.is_none() => target = Some(other.to_string()),
            other => words.push(other.to_string()),
        }
    }
    let Some(target) = target else {
        return usage_error("name the pane to type into");
    };
    if words.is_empty() {
        return usage_error("give the text to type");
    }
    let value = match call(&Request::Send {
        pane: pane_target(Some(target)),
        from: caller_pane(),
        text: words.join(" "),
        enter,
    }) {
        Ok(value) => value,
        Err(error) => return runtime_error(&error),
    };
    let send: SendResult = match serde_json::from_value(value) {
        Ok(send) => send,
        Err(error) => return runtime_error(&format!("unexpected reply: {error}")),
    };
    if json {
        return print_json(&send);
    }
    println!("{}", summary_line(&send.pane));
    println!("{}", if send.submitted { "sent" } else { "typed" });
    0
}

fn todo_cli(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("list" | "ls") => todo_list(&args[1..]),
        Some("show" | "get") => todo_show(&args[1..]),
        Some("add" | "new") => todo_add(&args[1..]),
        Some("update" | "edit") => todo_update(&args[1..]),
        Some("review") => todo_review(&args[1..]),
        Some("done" | "complete") => todo_done(&args[1..]),
        Some("remove" | "rm" | "delete") => todo_remove(&args[1..]),
        Some("-h" | "--help" | "help") => {
            println!("{USAGE}");
            0
        }
        _ => {
            eprintln!("{USAGE}");
            2
        }
    }
}

fn todo_list(args: &[String]) -> i32 {
    let mut status = None;
    let mut mine = false;
    let mut json = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--json" => json = true,
            "--mine" => mine = true,
            "--status" | "-s" => {
                let Some(raw) = iter.next() else {
                    return usage_error("--status needs todo, in_progress, or done");
                };
                let Some(parsed) = TodoStatus::parse(raw) else {
                    return usage_error(&format!(
                        "unknown status '{raw}': use todo, in_progress, or done"
                    ));
                };
                status = Some(parsed);
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return 0;
            }
            other => return usage_error(&format!("unknown option {other}")),
        }
    }
    let value = match call(&Request::TodoList {
        from: caller_pane(),
        status,
        mine,
    }) {
        Ok(value) => value,
        Err(error) => return runtime_error(&error),
    };
    if json {
        let Some(todos) = value.get("todos").filter(|v| v.is_array()) else {
            return runtime_error("unexpected reply: missing todos array");
        };
        println!("{todos}");
        return 0;
    }
    let todos: Vec<TodoItem> = match serde_json::from_value(value["todos"].clone()) {
        Ok(todos) => todos,
        Err(error) => return runtime_error(&format!("unexpected reply: {error}")),
    };
    if todos.is_empty() {
        println!("no todos");
        return 0;
    }
    for todo in &todos {
        println!("{}", todo_summary_line(todo));
    }
    0
}

fn todo_show(args: &[String]) -> i32 {
    let (json, rest) = match flags(args) {
        Ok(parsed) => parsed,
        Err(code) => return code,
    };
    let [raw_id] = rest.as_slice() else {
        return usage_error("give one todo id");
    };
    let Ok(id) = raw_id.parse::<i64>() else {
        return usage_error(&format!("invalid todo id '{raw_id}'"));
    };
    let value = match call(&Request::TodoShow {
        id,
        from: caller_pane(),
    }) {
        Ok(value) => value,
        Err(error) => return runtime_error(&error),
    };
    let item: TodoItem = match serde_json::from_value(value) {
        Ok(item) => item,
        Err(error) => return runtime_error(&format!("unexpected reply: {error}")),
    };
    if json {
        return print_json(&item);
    }
    println!("{}", todo_summary_line(&item));
    if !item.description.trim().is_empty() {
        println!("---");
        println!("{}", item.description.trim_end());
    }
    0
}

fn todo_add(args: &[String]) -> i32 {
    let mut title_words = Vec::new();
    let mut description = String::new();
    let mut status = TodoStatus::Todo;
    let mut bind = false;
    let mut json = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--json" => json = true,
            "--bind" => bind = true,
            "--desc" | "-d" => match iter.next() {
                Some(text) => description = text.clone(),
                None => return usage_error("--desc needs text"),
            },
            "--status" | "-s" => {
                let Some(raw) = iter.next() else {
                    return usage_error("--status needs todo, in_progress, or done");
                };
                let Some(parsed) = TodoStatus::parse(raw) else {
                    return usage_error(&format!(
                        "unknown status '{raw}': use todo, in_progress, or done"
                    ));
                };
                status = parsed;
            }
            "-h" | "--help" if title_words.is_empty() => {
                println!("{USAGE}");
                return 0;
            }
            other if other.starts_with('-') && title_words.is_empty() => {
                return usage_error(&format!("unknown option {other}"));
            }
            other => title_words.push(other),
        }
    }
    let title = title_words.join(" ").trim().to_string();
    if title.is_empty() {
        return usage_error("give a title for the todo");
    }
    let value = match call(&Request::TodoAdd {
        from: caller_pane(),
        title,
        description,
        status,
        bind,
    }) {
        Ok(value) => value,
        Err(error) => return runtime_error(&error),
    };
    let item: TodoItem = match serde_json::from_value(value) {
        Ok(item) => item,
        Err(error) => return runtime_error(&format!("unexpected reply: {error}")),
    };
    if json {
        return print_json(&item);
    }
    println!("{}", todo_summary_line(&item));
    0
}

fn todo_update(args: &[String]) -> i32 {
    let mut id = None;
    let mut title = None;
    let mut description = None;
    let mut status = None;
    let mut awaiting_review = None;
    let mut bind = false;
    let mut unbind = false;
    let mut json = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--json" => json = true,
            "--bind" => bind = true,
            "--unbind" => unbind = true,
            "--review" => awaiting_review = Some(true),
            "--no-review" => awaiting_review = Some(false),
            "--title" | "-t" => match iter.next() {
                Some(text) => title = Some(text.clone()),
                None => return usage_error("--title needs text"),
            },
            "--desc" | "-d" => match iter.next() {
                Some(text) => description = Some(text.clone()),
                None => return usage_error("--desc needs text"),
            },
            "--status" | "-s" => {
                let Some(raw) = iter.next() else {
                    return usage_error("--status needs todo, in_progress, or done");
                };
                let Some(parsed) = TodoStatus::parse(raw) else {
                    return usage_error(&format!(
                        "unknown status '{raw}': use todo, in_progress, or done"
                    ));
                };
                status = Some(parsed);
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return 0;
            }
            other if other.starts_with('-') => {
                return usage_error(&format!("unknown option {other}"));
            }
            other if id.is_none() => match other.parse::<i64>() {
                Ok(parsed) => id = Some(parsed),
                Err(_) => return usage_error(&format!("invalid todo id '{other}'")),
            },
            other => return usage_error(&format!("unexpected argument '{other}'")),
        }
    }
    let Some(id) = id else {
        return usage_error("give the todo id to update");
    };
    if bind && unbind {
        return usage_error("use either --bind or --unbind, not both");
    }
    let value = match call(&Request::TodoUpdate {
        id,
        from: caller_pane(),
        title,
        description,
        status,
        awaiting_review,
        bind,
        unbind,
    }) {
        Ok(value) => value,
        Err(error) => return runtime_error(&error),
    };
    let item: TodoItem = match serde_json::from_value(value) {
        Ok(item) => item,
        Err(error) => return runtime_error(&format!("unexpected reply: {error}")),
    };
    if json {
        return print_json(&item);
    }
    println!("{}", todo_summary_line(&item));
    0
}

fn todo_review(args: &[String]) -> i32 {
    let (json, rest) = match flags(args) {
        Ok(parsed) => parsed,
        Err(code) => return code,
    };
    let [raw_id] = rest.as_slice() else {
        return usage_error("give one todo id");
    };
    let Ok(id) = raw_id.parse::<i64>() else {
        return usage_error(&format!("invalid todo id '{raw_id}'"));
    };
    let value = match call(&Request::TodoUpdate {
        id,
        from: caller_pane(),
        title: None,
        description: None,
        status: Some(TodoStatus::InProgress),
        awaiting_review: Some(true),
        bind: false,
        unbind: false,
    }) {
        Ok(value) => value,
        Err(error) => return runtime_error(&error),
    };
    let item: TodoItem = match serde_json::from_value(value) {
        Ok(item) => item,
        Err(error) => return runtime_error(&format!("unexpected reply: {error}")),
    };
    if json {
        return print_json(&item);
    }
    println!("{}", todo_summary_line(&item));
    0
}

fn todo_done(args: &[String]) -> i32 {
    let (json, rest) = match flags(args) {
        Ok(parsed) => parsed,
        Err(code) => return code,
    };
    let [raw_id] = rest.as_slice() else {
        return usage_error("give one todo id");
    };
    let Ok(id) = raw_id.parse::<i64>() else {
        return usage_error(&format!("invalid todo id '{raw_id}'"));
    };
    let value = match call(&Request::TodoUpdate {
        id,
        from: caller_pane(),
        title: None,
        description: None,
        status: Some(TodoStatus::Done),
        awaiting_review: Some(false),
        bind: false,
        unbind: false,
    }) {
        Ok(value) => value,
        Err(error) => return runtime_error(&error),
    };
    let item: TodoItem = match serde_json::from_value(value) {
        Ok(item) => item,
        Err(error) => return runtime_error(&format!("unexpected reply: {error}")),
    };
    if json {
        return print_json(&item);
    }
    println!("{}", todo_summary_line(&item));
    0
}

fn todo_remove(args: &[String]) -> i32 {
    let (json, rest) = match flags(args) {
        Ok(parsed) => parsed,
        Err(code) => return code,
    };
    let [raw_id] = rest.as_slice() else {
        return usage_error("give one todo id");
    };
    let Ok(id) = raw_id.parse::<i64>() else {
        return usage_error(&format!("invalid todo id '{raw_id}'"));
    };
    let value = match call(&Request::TodoRemove {
        id,
        from: caller_pane(),
    }) {
        Ok(value) => value,
        Err(error) => return runtime_error(&error),
    };
    if json {
        println!("{value}");
        return 0;
    }
    println!("removed #{id}");
    0
}

fn todo_summary_line(todo: &TodoItem) -> String {
    let mut line = format!("#{} [{}]", todo.id, todo.status.as_str());
    if todo.awaiting_review {
        line.push_str(" [review]");
    }
    line.push_str(&format!(" {}", todo.title));
    if let Some(agent) = &todo.agent_id {
        line.push_str(&format!(" · {agent}"));
    }
    if let Some(pane) = todo.pane_id {
        line.push_str(&format!(" (pane {pane})"));
    }
    line
}

fn usage_error(message: &str) -> i32 {
    eprintln!("kumu: {message}\n{USAGE}");
    2
}

fn runtime_error(message: &str) -> i32 {
    eprintln!("kumu: {message}");
    1
}

fn print_json(value: &impl Serialize) -> i32 {
    match serde_json::to_string(value) {
        Ok(text) => {
            println!("{text}");
            0
        }
        Err(error) => runtime_error(&error.to_string()),
    }
}

/// The caller's own pane id: a direction is relative to it.
fn caller_pane() -> Option<u64> {
    std::env::var("KUMU_PANE")
        .ok()
        .and_then(|value| value.parse().ok())
}

/// A numeric argument is a pane id; anything else is read as a name.
fn pane_target(target: Option<String>) -> PaneTarget {
    match target.as_deref().map(str::parse::<u64>) {
        Some(Ok(id)) => PaneTarget::Id(id),
        _ => PaneTarget::Name(target.unwrap_or_else(|| "focused".into())),
    }
}

fn flags(args: &[String]) -> Result<(bool, Vec<String>), i32> {
    let mut json = false;
    let mut rest = Vec::new();
    for arg in args {
        match arg.as_str() {
            "--json" => json = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return Err(0);
            }
            other if other.starts_with('-') => {
                eprintln!("kumu: unknown option {other}\n{USAGE}");
                return Err(2);
            }
            other => rest.push(other.to_string()),
        }
    }
    Ok((json, rest))
}

/// Send one request and unwrap the reply envelope. Everything the caller gets
/// back is already checked for `ok` and for belonging to this pane's Kumu.
fn call(request: &Request) -> Result<Value, String> {
    call_at_instance(&socket_path(), request, caller_instance())
}

/// The pid of the Kumu this pane belongs to, exported as [`PID_ENV`]. A reply
/// from another pid means another Kumu instance serves the socket.
fn caller_instance() -> Option<u32> {
    std::env::var(PID_ENV)
        .ok()
        .and_then(|value| value.parse().ok())
}

fn call_at_instance(
    path: &Path,
    request: &Request,
    instance: Option<u32>,
) -> Result<Value, String> {
    let mut stream = UnixStream::connect(path).map_err(|error| {
        format!(
            "cannot reach kumu at {}: {error}\nrun this inside a Kumu pane, with Kumu open",
            path.display()
        )
    })?;
    let body = serde_json::to_string(&RequestLine {
        pid: instance,
        request: request.clone(),
    })
    .map_err(|error| error.to_string())?;
    let _ = stream.set_write_timeout(Some(REPLY_TIMEOUT));
    stream
        .write_all(format!("{body}\n").as_bytes())
        .and_then(|()| stream.flush())
        .map_err(|error| format!("cannot send the request: {error}"))?;
    // The server gives up on a busy app after REPLY_TIMEOUT (or wait timeout
    // for `pane wait`); wait a little past that so its error reaches the user
    // instead of a silent hang.
    let read_timeout = match request {
        Request::Wait { timeout_secs, .. } => {
            Duration::from_secs((*timeout_secs).clamp(1, MAX_WAIT_TIMEOUT_SECS) + 10)
        }
        _ => REPLY_TIMEOUT + Duration::from_secs(2),
    };
    let _ = stream.set_read_timeout(Some(read_timeout));
    let mut line = String::new();
    let n = BufReader::new(stream.take(MAX_RESPONSE_BYTES))
        .read_line(&mut line)
        .map_err(|error| format!("kumu did not answer: {error}"))?;
    if n == 0 {
        return Err("kumu closed the connection before replying".to_string());
    }
    let reply: Value =
        serde_json::from_str(line.trim()).map_err(|error| format!("bad reply: {error}"))?;
    if let Some(expected) = instance
        && reply["pid"].as_u64() != Some(u64::from(expected))
    {
        // Another Kumu answered: its pane ids mean nothing here, so a read or
        // a send would act on the wrong window.
        let served_by = reply["pid"]
            .as_u64()
            .map_or_else(|| "another process".to_string(), |pid| format!("pid {pid}"));
        return Err(format!(
            "the control socket at {} belongs to another Kumu ({served_by}); this pane belongs to pid {expected}",
            path.display()
        ));
    }
    if reply["ok"].as_bool() != Some(true) {
        let kind = reply["error"].as_str().unwrap_or("error");
        let message = reply["message"].as_str().unwrap_or("no detail");
        return Err(format!("{kind}: {message}"));
    }
    Ok(reply["result"].clone())
}

fn summary_line(pane: &PaneSummary) -> String {
    let mut line = format!("pane {}", pane.id);
    line.push_str(&format!(" · {}", pane.project_name));
    if !pane.title.is_empty() {
        line.push_str(&format!(" · {}", pane.title));
    }
    if let Some(agent) = &pane.agent {
        line.push_str(&format!(" · agent {agent}"));
    }
    line.push_str(&format!(" · {}", pane.cwd));
    line.push_str(&format!(" · {}x{}", pane.cols, pane.rows));
    if pane.focused {
        line.push_str(" · focused");
    }
    if pane.selected {
        line.push_str(" · shown");
    }
    if pane.exited {
        line.push_str(" · exited");
    }
    line
}

/// The last `count` lines of `text`; `count == 0` keeps nothing.
fn last_lines(text: &str, count: usize) -> String {
    if count == 0 {
        return String::new();
    }
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(count)..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The test's socket is served by this process, so that is the instance a
    /// client should expect.
    fn call_here(path: &Path, request: &Request) -> Result<Value, String> {
        call_at_instance(path, request, Some(std::process::id()))
    }

    #[test]
    fn a_request_line_carries_the_instance_only_when_known() {
        let request = Request::Panes;
        let plain = serde_json::to_string(&RequestLine {
            pid: None,
            request: request.clone(),
        })
        .expect("request line");
        assert_eq!(plain, r#"{"op":"panes"}"#);

        let known = serde_json::to_string(&RequestLine {
            pid: Some(7),
            request,
        })
        .expect("request line");
        let decoded: RequestLine = serde_json::from_str(&known).expect("decode");
        assert_eq!(decoded.pid, Some(7));
        assert!(matches!(decoded.request, Request::Panes));
    }

    #[test]
    fn a_read_request_round_trips_as_one_json_line() {
        let request = Request::Read {
            pane: PaneTarget::Name("right".into()),
            from: Some(7),
            history: 200,
        };
        let text = serde_json::to_string(&request).expect("request");
        assert_eq!(
            text,
            r#"{"op":"read","pane":"right","from":7,"history":200}"#
        );
        let decoded: Request = serde_json::from_str(&text).expect("decode");
        assert!(matches!(
            decoded,
            Request::Read {
                pane: PaneTarget::Name(name),
                from: Some(7),
                history: 200,
            } if name == "right"
        ));
    }

    #[test]
    fn a_pane_id_target_survives_the_same_line() {
        let request: Request = serde_json::from_str(r#"{"op":"read","pane":3}"#).expect("decode");
        assert!(matches!(
            request,
            Request::Read {
                pane: PaneTarget::Id(3),
                from: None,
                history: 0,
            }
        ));
    }

    #[test]
    fn the_tail_keeps_the_newest_lines() {
        assert_eq!(last_lines("a\nb\nc\nd", 2), "c\nd");
        assert_eq!(last_lines("a\nb", 9), "a\nb");
        assert_eq!(last_lines("a\nb", 0), "");
    }

    fn summary(id: u64) -> PaneSummary {
        PaneSummary {
            id,
            project: 1,
            project_name: "kumu".into(),
            title: "zsh".into(),
            command: "zsh".into(),
            cwd: "/tmp".into(),
            agent: None,
            focused: id == 3,
            selected: true,
            cols: 120,
            rows: 38,
            exited: false,
        }
    }

    /// The window side of the channel is a plain receiver here: the point is
    /// that a client really gets a reply over the socket, and that the request
    /// reached the app intact.
    #[test]
    fn a_client_reads_a_list_and_a_pane_over_the_socket() {
        use futures::StreamExt;

        let dir = std::env::temp_dir().join(format!(
            "kumu-control-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("socket dir");
        let path = dir.join("control.sock");
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        let handle = start_at(path.clone(), tx).expect("bind the socket");

        let answering = std::thread::spawn(move || {
            while let Some(command) = futures::executor::block_on(rx.next()) {
                match command {
                    Command::Panes { reply } => {
                        reply.send(Ok(vec![summary(3)])).expect("list reply");
                    }
                    Command::Read {
                        target,
                        from,
                        history,
                        reply,
                    } => {
                        assert!(
                            matches!(&target, PaneTarget::Name(name) if name == "right"),
                            "{target:?}"
                        );
                        assert_eq!(from, Some(3));
                        assert_eq!(history, 200, "history is passed through");
                        reply
                            .send(Ok(PaneContent {
                                pane: summary(4),
                                text: "hello".into(),
                            }))
                            .expect("read reply");
                    }
                    Command::Split {
                        direction,
                        agent,
                        from,
                        prompt,
                        cancelled: _,
                        reply,
                    } => {
                        assert_eq!(direction, "right");
                        assert_eq!(agent, "codex");
                        assert_eq!(from, Some(3));
                        assert_eq!(prompt.as_deref(), Some("review the diff"));
                        reply
                            .send(Ok(SplitResult { pane: summary(5) }))
                            .expect("split reply");
                    }
                    Command::Send {
                        target,
                        from,
                        text,
                        enter,
                        reply,
                    } => {
                        assert!(matches!(&target, PaneTarget::Id(5)), "{target:?}");
                        assert_eq!(from, Some(3));
                        assert_eq!(text, "review the diff");
                        assert!(enter, "Enter follows the text by default");
                        reply
                            .send(Ok(SendResult {
                                pane: summary(5),
                                submitted: enter,
                            }))
                            .expect("send reply");
                    }
                    Command::TodoList {
                        from,
                        status,
                        mine,
                        reply,
                    } => {
                        assert_eq!(from, Some(3));
                        assert_eq!(status, Some(TodoStatus::InProgress));
                        assert!(mine);
                        reply
                            .send(Ok(vec![TodoItem {
                                id: 9,
                                project_id: 1,
                                title: "Ship TODO board".into(),
                                description: String::new(),
                                status: TodoStatus::InProgress,
                                position: 0,
                                pane_id: Some(3),
                                agent_id: Some("claude".into()),
                                agent_session_id: None,
                                awaiting_review: true,
                                created_at: "2026-11-21 00:00:00".into(),
                                updated_at: "2026-11-21 00:00:00".into(),
                                completed_at: None,
                            }]))
                            .expect("todo list reply");
                    }
                    Command::Close {
                        target,
                        allow_self,
                        reply,
                        ..
                    } => {
                        assert!(matches!(target, PaneTarget::Id(5)));
                        assert!(!allow_self);
                        reply
                            .send(Ok(CloseResult {
                                pane: 5,
                                closed: true,
                            }))
                            .expect("close reply");
                    }
                    Command::Wait {
                        target,
                        timeout_secs,
                        next,
                        reply,
                        ..
                    } => {
                        assert!(matches!(target, PaneTarget::Id(5)));
                        assert_eq!(timeout_secs, 30);
                        assert!(next);
                        reply
                            .send(Ok(WaitResult {
                                pane: PaneSummary {
                                    id: 5,
                                    project: 1,
                                    project_name: "kumu".into(),
                                    title: "codex".into(),
                                    command: "codex".into(),
                                    cwd: "/tmp/kumu".into(),
                                    agent: Some("codex".into()),
                                    focused: false,
                                    selected: true,
                                    cols: 80,
                                    rows: 24,
                                    exited: false,
                                },
                                state: "completed".into(),
                                elapsed_ms: 420,
                            }))
                            .expect("wait reply");
                    }
                    Command::Scroll {
                        target,
                        from,
                        direction,
                        lines,
                        col,
                        row,
                        reply,
                    } => {
                        assert!(matches!(target, PaneTarget::Name(ref s) if s == "right"));
                        assert_eq!(from, Some(3));
                        assert_eq!(direction, ScrollDirection::Up);
                        assert_eq!(lines, Some(15));
                        assert_eq!(col, Some(20));
                        assert_eq!(row, Some(10));
                        reply
                            .send(Ok(ScrollResult {
                                pane: summary(4),
                                direction,
                                lines: 15,
                                moved: Some(-15),
                            }))
                            .expect("scroll reply");
                    }
                    Command::TodoShow { .. }
                    | Command::TodoAdd { .. }
                    | Command::TodoUpdate { .. }
                    | Command::TodoRemove { .. } => {}
                }
            }
        });

        let list = call_here(&path, &Request::Panes).expect("list");
        assert_eq!(list["panes"][0]["id"], 3);
        assert_eq!(list["panes"][0]["projectName"], "kumu");

        let read = call_here(
            &path,
            &Request::Read {
                pane: PaneTarget::Name("right".into()),
                from: Some(3),
                history: 200,
            },
        )
        .expect("read");
        assert_eq!(read["text"], "hello");
        assert_eq!(read["pane"]["id"], 4);

        let scrolled = call_here(
            &path,
            &Request::Scroll {
                pane: PaneTarget::Name("right".into()),
                from: Some(3),
                direction: ScrollDirection::Up,
                lines: Some(15),
                col: Some(20),
                row: Some(10),
            },
        )
        .expect("scroll");
        assert_eq!(scrolled["pane"]["id"], 4);
        assert_eq!(scrolled["direction"], "up");
        assert_eq!(scrolled["lines"], 15);
        assert_eq!(scrolled["moved"], -15);

        let bad_scroll = call_here(
            &path,
            &Request::Scroll {
                pane: PaneTarget::Name("right".into()),
                from: Some(3),
                direction: ScrollDirection::Down,
                lines: Some(MAX_SCROLL_LINES + 1),
                col: None,
                row: None,
            },
        )
        .expect_err("out-of-range scroll lines rejected");
        assert!(bad_scroll.contains("between 1 and"), "{bad_scroll}");

        let split = call_here(
            &path,
            &Request::Split {
                direction: "right".into(),
                agent: "codex".into(),
                from: Some(3),
                prompt: Some("review the diff".into()),
            },
        )
        .expect("split");
        assert_eq!(split["pane"]["id"], 5);

        let send = call_here(
            &path,
            &Request::Send {
                pane: PaneTarget::Id(5),
                from: Some(3),
                text: "review the diff".into(),
                enter: true,
            },
        )
        .expect("send");
        assert_eq!(send["submitted"], true);

        let todos = call_here(
            &path,
            &Request::TodoList {
                from: Some(3),
                status: Some(TodoStatus::InProgress),
                mine: true,
            },
        )
        .expect("todo list");
        assert_eq!(todos["todos"][0]["id"], 9);
        assert_eq!(todos["todos"][0]["awaitingReview"], true);

        let waited = call_here(
            &path,
            &Request::Wait {
                pane: PaneTarget::Id(5),
                from: Some(3),
                timeout_secs: 30,
                next: true,
            },
        )
        .expect("wait");
        assert_eq!(waited["state"], "completed");
        assert_eq!(waited["elapsedMs"], 420);

        let closed = call_here(
            &path,
            &Request::Close {
                pane: PaneTarget::Id(5),
                from: Some(3),
                allow_self: false,
            },
        )
        .expect("close");
        assert_eq!(closed["pane"], 5);
        assert_eq!(closed["closed"], true);

        // A pane of another Kumu must not read this one's panes: the reply
        // carries the serving pid, so the client can refuse the answer.
        let foreign = call_at_instance(&path, &Request::Panes, Some(std::process::id() + 1))
            .expect_err("another instance is refused");
        assert!(foreign.contains("another Kumu"), "{foreign}");

        drop(handle);
        answering
            .join()
            .expect("the answering side ends with the socket");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// accept() inherits the listener's nonblocking flag on macOS, so a client
    /// whose request lands a moment after the connection must still be read as
    /// one request instead of an empty line.
    #[test]
    fn a_late_request_is_not_read_as_an_empty_one() {
        use futures::StreamExt;

        let dir = std::env::temp_dir().join(format!(
            "kumu-late-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("socket dir");
        let path = dir.join("control.sock");
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        let handle = start_at(path.clone(), tx).expect("bind the socket");
        let answering = std::thread::spawn(move || {
            while let Some(command) = futures::executor::block_on(rx.next()) {
                if let Command::Panes { reply } = command {
                    let _ = reply.send(Ok(vec![summary(3)]));
                }
            }
        });

        let mut stream = UnixStream::connect(&path).expect("connect");
        std::thread::sleep(Duration::from_millis(300));
        stream
            .write_all(b"{\"op\":\"panes\"}\n")
            .expect("write request");
        let mut reply = String::new();
        BufReader::new(stream)
            .read_line(&mut reply)
            .expect("read reply");
        assert!(reply.contains("\"ok\":true"), "{reply}");

        drop(handle);
        answering
            .join()
            .expect("the answering side ends with the socket");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_pane_scroll_handles_direction_target_and_bounds() {
        let s = |xs: &[&str]| xs.iter().map(|v| (*v).to_string()).collect::<Vec<_>>();

        let ScrollParse::Parsed(parsed) = parse_pane_scroll(&s(&["up"])) else {
            panic!("expected parsed up with default target");
        };
        assert_eq!(
            parsed,
            ScrollCliArgs {
                direction: ScrollDirection::Up,
                target: None,
                lines: None,
                col: None,
                row: None,
                json: false,
            }
        );

        // `up` as direction and `up` as target must not be ambiguous.
        let ScrollParse::Parsed(parsed) =
            parse_pane_scroll(&s(&["down", "up", "--lines", "12", "--col", "8", "--row", "4", "--json"]))
        else {
            panic!("expected parsed down targeting pane above");
        };
        assert_eq!(
            parsed,
            ScrollCliArgs {
                direction: ScrollDirection::Down,
                target: Some("up".into()),
                lines: Some(12),
                col: Some(8),
                row: Some(4),
                json: true,
            }
        );

        assert!(matches!(
            parse_pane_scroll(&s(&["right"])),
            ScrollParse::Error(_)
        ));
        assert!(matches!(
            parse_pane_scroll(&s(&["up", "23", "--lines", "0"])),
            ScrollParse::Error(_)
        ));
        assert!(matches!(
            parse_pane_scroll(&s(&["up", "23", "--lines", "501"])),
            ScrollParse::Error(_)
        ));
    }
}
