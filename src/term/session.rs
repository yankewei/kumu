//! The thread that owns one PTY and its terminal emulator.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender, SyncSender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use futures::channel::mpsc::UnboundedReceiver;
use portable_pty::PtySize;

use super::Hyperlink;
use super::pty::{self, Pty};
use super::vt::{
    KeyInput, MAX_NOTIFICATIONS, MouseInput, MouseMods, SelectStep, Snapshot, TermColors,
    TermNotification, Vt,
};

/// How long a content read waits for the terminal thread.
const CONTENT_TIMEOUT: Duration = Duration::from_secs(5);

/// A hover probe answers a click or a highlight; a terminal buried in output
/// may answer late, but the pointer has moved on by then.
const LINK_TIMEOUT: Duration = Duration::from_millis(500);

/// Maximum wait for selection text on mouse release.
const SELECT_TIMEOUT: Duration = Duration::from_millis(200);

/// Backpressure limit for unread PTY output chunks before `kumu-pty` pauses.
const MAX_PTY_BUFFER_BYTES: usize = 512 * 1024;

/// Maximum PTY bytes drained in one batch before publishing a frame.
const MAX_BATCH_BYTES: usize = 256 * 1024;

/// Maximum time spent coalescing queued PTY chunks in one batch.
const MAX_BATCH_TIME: Duration = Duration::from_millis(4);

/// Bounded capacity of the asynchronous PTY writer queue.
const PTY_WRITE_QUEUE: usize = 64;

#[cfg(unix)]
unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

#[cfg(unix)]
fn terminate_process_group(child: &mut Box<dyn portable_pty::Child + Send + Sync>) {
    const SIGHUP: i32 = 1;
    const SIGKILL: i32 = 9;
    if let Some(pid) = child.process_id()
        && let Ok(pgid) = i32::try_from(pid)
        && pgid > 0
    {
        unsafe {
            kill(-pgid, SIGHUP);
        }
        // Give the process group a brief window to react to SIGHUP before SIGKILL.
        let deadline = Instant::now() + Duration::from_millis(40);
        while Instant::now() < deadline {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        unsafe {
            kill(-pgid, SIGKILL);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(not(unix))]
fn terminate_process_group(child: &mut Box<dyn portable_pty::Child + Send + Sync>) {
    let _ = child.kill();
    let _ = child.wait();
}

struct Live {
    screen: Mutex<Snapshot>,
    notifications: Mutex<VecDeque<TermNotification>>,
    queued: AtomicBool,
    /// The PTY child (the shell), or 0 before it is spawned.
    pid: AtomicU64,
    /// Per-pane random token authenticating `kumu-session:` OSC reports.
    session_token: String,
    wake: futures::channel::mpsc::UnboundedSender<()>,
}

impl Live {
    fn publish(&self, snapshot: Snapshot, force_wake: bool) {
        *self.screen.lock().unwrap_or_else(|e| e.into_inner()) = snapshot;
        let was_queued = self.queued.swap(true, Ordering::SeqCst);
        if !was_queued || force_wake {
            let _ = self.wake.unbounded_send(());
        }
    }

    fn wake_for_notification(&self) {
        self.queued.store(true, Ordering::SeqCst);
        let _ = self.wake.unbounded_send(());
    }
}

struct InboxState {
    control: VecDeque<Msg>,
    pty_bytes: VecDeque<Vec<u8>>,
    pty_queued_bytes: usize,
    eof: bool,
    shutdown: bool,
    closed: bool,
}

/// Priority mailbox for `kumu-term`: control/UI commands always preempt PTY
/// output floods, while PTY reads apply bounded backpressure so a flooding
/// child cannot OOM Kumu.
struct Inbox {
    state: Mutex<InboxState>,
    item_cv: Condvar,
    space_cv: Condvar,
}

enum NextWork {
    Shutdown,
    Control(Msg),
    PtyBatch(Vec<Vec<u8>>),
    Eof,
    Timeout,
}

impl Inbox {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(InboxState {
                control: VecDeque::new(),
                pty_bytes: VecDeque::new(),
                pty_queued_bytes: 0,
                eof: false,
                shutdown: false,
                closed: false,
            }),
            item_cv: Condvar::new(),
            space_cv: Condvar::new(),
        })
    }

    fn send_control(&self, msg: Msg) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.closed {
            return;
        }
        // Coalesce queued resizes so animated panel transitions and fast window
        // drags never back up stale intermediate column widths in the VT thread.
        if let Msg::Resize { .. } = msg
            && let Some(existing) = state
                .control
                .iter_mut()
                .rev()
                .find(|queued| matches!(queued, Msg::Resize { .. }))
        {
            *existing = msg;
        } else {
            state.control.push_back(msg);
        }
        self.item_cv.notify_one();
    }

    fn send_shutdown(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.shutdown = true;
        state.closed = true;
        self.item_cv.notify_one();
        self.space_cv.notify_all();
    }

    fn close(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.closed = true;
        self.space_cv.notify_all();
    }

    fn push_pty(&self, chunk: Vec<u8>) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        while state.pty_queued_bytes >= MAX_PTY_BUFFER_BYTES && !state.closed {
            state = self.space_cv.wait(state).unwrap_or_else(|e| e.into_inner());
        }
        if state.closed {
            return false;
        }
        state.pty_queued_bytes += chunk.len();
        state.pty_bytes.push_back(chunk);
        self.item_cv.notify_one();
        true
    }

    fn push_eof(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.eof = true;
        self.item_cv.notify_one();
    }

    fn next(&self, timeout: Option<Duration>) -> NextWork {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let deadline = timeout.map(|d| Instant::now() + d);
        loop {
            if state.shutdown {
                return NextWork::Shutdown;
            }
            if let Some(msg) = state.control.pop_front() {
                return NextWork::Control(msg);
            }
            if !state.pty_bytes.is_empty() {
                let mut batch = Vec::new();
                let mut total = 0usize;
                while let Some(chunk) = state.pty_bytes.pop_front() {
                    let len = chunk.len();
                    state.pty_queued_bytes = state.pty_queued_bytes.saturating_sub(len);
                    total += len;
                    batch.push(chunk);
                    if total >= MAX_BATCH_BYTES {
                        break;
                    }
                }
                self.space_cv.notify_all();
                return NextWork::PtyBatch(batch);
            }
            if state.eof {
                state.eof = false;
                return NextWork::Eof;
            }
            match deadline {
                Some(until) => {
                    let remaining = until.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return NextWork::Timeout;
                    }
                    let (guard, res) = self
                        .item_cv
                        .wait_timeout(state, remaining)
                        .unwrap_or_else(|e| e.into_inner());
                    state = guard;
                    if res.timed_out()
                        && !state.shutdown
                        && state.control.is_empty()
                        && state.pty_bytes.is_empty()
                        && !state.eof
                    {
                        return NextWork::Timeout;
                    }
                }
                None => {
                    state = self.item_cv.wait(state).unwrap_or_else(|e| e.into_inner());
                }
            }
        }
    }
}

enum Msg {
    /// Bytes to write to the PTY (IME commit).
    Write(Vec<u8>),
    /// Clipboard text, encoded for the program's paste mode.
    Paste(Vec<u8>),
    Key(KeyInput),
    /// Mouse report for a program that tracks the mouse.
    Mouse(MouseInput),
    /// Selection gesture; optional reply carries the selected text after Release.
    Select {
        step: SelectStep,
        col: u16,
        row: u16,
        x: f32,
        y: f32,
        cell_w: f32,
        height: f32,
        reply: Option<Sender<Option<String>>>,
    },
    ClearSelection,
    SelectionText(Sender<Option<String>>),
    /// The OSC 8 hyperlink on a viewport cell, with the cells it covers.
    HyperlinkAt {
        col: u16,
        row: u16,
        reply: Sender<Option<Hyperlink>>,
    },
    /// Find bar: set the needle (empty clears), or walk matches.
    SearchSet(String),
    SearchStep {
        older: bool,
    },
    SearchClear,
    /// The screen and up to `history` rows above it, as plain text.
    Content {
        history: usize,
        reply: Sender<Option<String>>,
    },
    /// Scroll the viewport (primary screen) or send arrows / mouse wheel.
    Scroll {
        lines: isize,
        /// Local pixel position inside the grid, for mouse-wheel reports.
        x: f32,
        y: f32,
        cell_w: f32,
        cell_h: f32,
        mods: MouseMods,
    },
    Resize {
        cols: u16,
        rows: u16,
        cell_w: u32,
        cell_h: u32,
    },
    Colors(TermColors),
}

/// Sends shutdown when the last [`Term`] handle is dropped.
struct Close {
    inbox: Arc<Inbox>,
}

impl Drop for Close {
    fn drop(&mut self) {
        self.inbox.send_shutdown();
    }
}

/// Handle to a running terminal. Dropping the last handle stops the process.
#[derive(Clone)]
pub struct Term {
    inbox: Arc<Inbox>,
    live: Arc<Live>,
    /// Keeps the process alive until the last handle is dropped.
    _close: Arc<Close>,
}

impl Term {
    /// Start `program` in `cwd` on a new PTY. The receiver wakes when the grid changes.
    #[cfg(test)]
    pub fn launch(
        program: String,
        args: Vec<String>,
        cwd: PathBuf,
        colors: TermColors,
    ) -> (Self, UnboundedReceiver<()>) {
        Self::launch_in_pane(None, program, args, cwd, colors)
    }

    /// Start a program that belongs to a Kumu pane: the pane id travels in
    /// `KUMU_PANE`, so a program can tell which pane it runs in and ask the
    /// app about its neighbours.
    pub fn launch_in_pane(
        pane: Option<u64>,
        program: String,
        args: Vec<String>,
        cwd: PathBuf,
        colors: TermColors,
    ) -> (Self, UnboundedReceiver<()>) {
        let inbox = Inbox::new();
        let (wake, wake_rx) = futures::channel::mpsc::unbounded();
        let command = std::path::Path::new(&program)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or(program.clone());
        let cwd_text = cwd.display().to_string();
        let session_token = pty::generate_session_token();
        let live = Arc::new(Live {
            screen: Mutex::new(Snapshot::blank_with(
                &command, &cwd_text, colors.fg, colors.bg,
            )),
            notifications: Mutex::new(VecDeque::new()),
            queued: AtomicBool::new(false),
            pid: AtomicU64::new(0),
            session_token: session_token.clone(),
            wake,
        });
        let thread_inbox = Arc::clone(&inbox);
        let thread_live = Arc::clone(&live);
        let launch = Launch {
            pane,
            program,
            args,
            cwd,
            command: command.clone(),
            cwd_text: cwd_text.clone(),
            colors,
            session_token,
        };
        if let Err(error) = std::thread::Builder::new()
            .name("kumu-term".into())
            .spawn(move || run(launch, thread_inbox, thread_live))
        {
            // Thread creation can fail under memory/thread pressure. Keep the
            // same error channel every other spawn failure uses instead of
            // taking the whole app down.
            let mut snapshot = Snapshot::blank_with(&command, &cwd_text, colors.fg, colors.bg);
            snapshot.exited = true;
            snapshot.spawn_error = Some(format!("无法创建终端线程：{error}"));
            live.publish(snapshot, true);
            inbox.close();
        }
        let _close = Arc::new(Close {
            inbox: Arc::clone(&inbox),
        });
        (
            Self {
                inbox,
                live,
                _close,
            },
            wake_rx,
        )
    }

    pub fn snapshot(&self) -> Snapshot {
        self.live
            .screen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// The per-pane token that authenticates `kumu-session:` reports from this
    /// terminal's hooks and extensions.
    pub fn session_token(&self) -> &str {
        &self.live.session_token
    }

    /// The title the program set with OSC 0/2, without copying the screen. It
    /// reads the way [`Snapshot::label`] does: control characters removed, and
    /// the command when no title was set.
    pub fn label(&self) -> String {
        let screen = self.live.screen.lock().unwrap_or_else(|e| e.into_inner());
        let title: String = screen.title.chars().filter(|ch| !ch.is_control()).collect();
        let title = title.trim();
        if !title.is_empty() {
            return title.to_string();
        }
        screen.command.clone()
    }

    /// Consume attention separately from the screen so redraws cannot replay it.
    pub fn take_notification(&self) -> Option<TermNotification> {
        self.live
            .notifications
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
    }

    /// The pid of the PTY child, once it has spawned.
    pub fn child_pid(&self) -> Option<u32> {
        let pid = self.live.pid.load(Ordering::Acquire);
        (pid != 0).then_some(pid as u32)
    }

    /// The UI has started handling a wake. A publish that arrives after this
    /// sends another one.
    pub fn wake_handled(&self) {
        self.live.queued.store(false, Ordering::SeqCst);
    }

    pub fn key(&self, input: KeyInput) {
        self.inbox.send_control(Msg::Key(input));
    }

    /// Write raw bytes to the PTY (finished IME text).
    pub fn write(&self, bytes: impl Into<Vec<u8>>) {
        let bytes = bytes.into();
        if !bytes.is_empty() {
            self.inbox.send_control(Msg::Write(bytes));
        }
    }

    /// Paste clipboard text, framed for the program's bracketed paste mode.
    pub fn paste(&self, text: impl Into<Vec<u8>>) {
        let text = text.into();
        if !text.is_empty() {
            self.inbox.send_control(Msg::Paste(text));
        }
    }

    pub fn mouse(&self, input: MouseInput) {
        self.inbox.send_control(Msg::Mouse(input));
    }

    /// Drive a selection gesture. On [`SelectStep::Release`], returns the
    /// selected text (copy-on-select) once the session thread applies it.
    ///
    /// This blocks until the session thread replies (or 200ms), so call it
    /// from a background task, not from event dispatch.
    #[allow(clippy::too_many_arguments)]
    pub fn select(
        &self,
        step: SelectStep,
        col: u16,
        row: u16,
        x: f32,
        y: f32,
        cell_w: f32,
        height: f32,
    ) -> Option<String> {
        let (reply, wait) = if matches!(step, SelectStep::Release) {
            let (tx, rx) = mpsc::channel();
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };
        self.inbox.send_control(Msg::Select {
            step,
            col,
            row,
            x,
            y,
            cell_w,
            height,
            reply,
        });
        wait.and_then(|rx| rx.recv_timeout(SELECT_TIMEOUT).ok())
            .flatten()
    }

    pub fn clear_selection(&self) {
        self.inbox.send_control(Msg::ClearSelection);
    }

    /// The OSC 8 hyperlink on a viewport cell, with the cells it covers.
    ///
    /// Blocks until the session thread answers, so callers run on a background
    /// task; a terminal busy with output drops the probe instead of holding the
    /// pointer.
    pub fn hyperlink_at(&self, col: u16, row: u16) -> Option<Hyperlink> {
        let (tx, rx) = mpsc::channel();
        self.inbox.send_control(Msg::HyperlinkAt {
            col,
            row,
            reply: tx,
        });
        rx.recv_timeout(LINK_TIMEOUT).ok().flatten()
    }

    /// Search the terminal for `needle` and select the newest match. An empty
    /// needle closes the find bar's highlights.
    pub fn search_set(&self, needle: impl Into<String>) {
        self.inbox.send_control(Msg::SearchSet(needle.into()));
    }

    /// Move the selected match toward older (`older`) or newer content.
    pub fn search_step(&self, older: bool) {
        self.inbox.send_control(Msg::SearchStep { older });
    }

    pub fn search_clear(&self) {
        self.inbox.send_control(Msg::SearchClear);
    }

    /// Currently selected plain text, if any.
    pub fn selection_text(&self) -> Option<String> {
        let (tx, rx) = mpsc::channel();
        self.inbox.send_control(Msg::SelectionText(tx));
        rx.recv_timeout(CONTENT_TIMEOUT).ok().flatten()
    }

    /// The screen and up to `history` rows of scrollback, as plain text.
    pub fn content(&self, history: usize) -> Option<String> {
        let (tx, rx) = mpsc::channel();
        self.inbox.send_control(Msg::Content { history, reply: tx });
        rx.recv_timeout(CONTENT_TIMEOUT).ok().flatten()
    }

    pub fn scroll(&self, lines: isize, x: f32, y: f32, cell_w: f32, cell_h: f32, mods: MouseMods) {
        if lines == 0 {
            return;
        }
        self.inbox.send_control(Msg::Scroll {
            lines,
            x,
            y,
            cell_w,
            cell_h,
            mods,
        });
    }

    pub fn resize(&self, cols: u16, rows: u16, cell_w: u32, cell_h: u32) {
        self.inbox.send_control(Msg::Resize {
            cols,
            rows,
            cell_w,
            cell_h,
        });
    }

    pub fn set_colors(&self, colors: TermColors) {
        self.inbox.send_control(Msg::Colors(colors));
    }
}

/// What a terminal thread needs at start: the command, where it runs, and the
/// labels its snapshots carry.
struct Launch {
    /// Pane this process belongs to, exported as `KUMU_PANE`.
    pane: Option<u64>,
    program: String,
    args: Vec<String>,
    cwd: PathBuf,
    command: String,
    cwd_text: String,
    colors: TermColors,
    session_token: String,
}

fn run(launch: Launch, inbox: Arc<Inbox>, live: Arc<Live>) {
    let Launch {
        pane,
        program,
        args,
        cwd,
        command,
        cwd_text,
        colors,
        session_token,
    } = launch;
    let size = PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 80 * 8,
        pixel_height: 24 * 16,
    };
    let mut pty = match pty::open(&program, &args, &cwd, size, pane, &session_token) {
        Ok(pty) => pty,
        Err(error) => {
            let mut snapshot = Snapshot::blank_with(&command, &cwd_text, colors.fg, colors.bg);
            snapshot.exited = true;
            snapshot.spawn_error = Some(error.to_string());
            live.publish(snapshot, true);
            inbox.close();
            return;
        }
    };
    if let Some(pid) = pty.child.process_id() {
        live.pid.store(pid as u64, Ordering::Release);
    }
    let mut reader = std::mem::replace(&mut pty.reader, Box::new(std::io::empty()));
    let mut writer = std::mem::replace(&mut pty.writer, Box::new(std::io::sink()));
    let (write_tx, write_rx) = mpsc::sync_channel::<Vec<u8>>(PTY_WRITE_QUEUE);
    let _ = std::thread::Builder::new()
        .name("kumu-pty-write".into())
        .spawn(move || {
            while let Ok(bytes) = write_rx.recv() {
                if writer.write_all(&bytes).is_err() || writer.flush().is_err() {
                    break;
                }
            }
        });

    let reader_inbox = Arc::clone(&inbox);
    let _ = std::thread::Builder::new()
        .name("kumu-pty".into())
        .spawn(move || read_pty(&mut reader, &reader_inbox));

    let vt = match Vt::with_colors(80, 24, colors) {
        Ok(vt) => vt,
        Err(error) => {
            // The emulator could not be created, so the pane has no way to
            // draw this process. Report it like any other spawn failure and
            // stop the child instead of leaving an unusable thread around.
            let mut snapshot = Snapshot::blank_with(&command, &cwd_text, colors.fg, colors.bg);
            snapshot.exited = true;
            snapshot.spawn_error = Some(error);
            live.publish(snapshot, true);
            inbox.close();
            terminate_process_group(&mut pty.child);
            return;
        }
    };
    let mut session = Running {
        pty,
        write_tx,
        vt,
        command,
        cwd: cwd_text,
        exited: false,
        live,
    };
    session.vt.resize(80, 24, 8, 16);
    session.publish(false);

    session.pump(&inbox);
    inbox.close();
    drop(session.write_tx);
    terminate_process_group(&mut session.pty.child);
}

fn read_pty(reader: &mut dyn Read, inbox: &Inbox) {
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => {
                inbox.push_eof();
                break;
            }
            Ok(n) => {
                if !inbox.push_pty(buf[..n].to_vec()) {
                    break;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => {
                inbox.push_eof();
                break;
            }
        }
    }
}

struct Running {
    pty: Pty,
    write_tx: SyncSender<Vec<u8>>,
    vt: Vt,
    command: String,
    cwd: String,
    exited: bool,
    live: Arc<Live>,
}

impl Running {
    fn write_pty(&self, bytes: Vec<u8>) {
        if !bytes.is_empty() {
            // Never block `kumu-term` on a wedged child's stdin.
            let _ = self.write_tx.try_send(bytes);
        }
    }

    fn pump(&mut self, inbox: &Inbox) {
        loop {
            let work = inbox.next(self.vt.hold_wait());
            match work {
                NextWork::Timeout => self.publish(false),
                NextWork::PtyBatch(chunks) => {
                    let started = Instant::now();
                    for chunk in chunks {
                        self.vt.write(&chunk);
                        self.flush_replies();
                        if started.elapsed() >= MAX_BATCH_TIME {
                            break;
                        }
                    }
                    if self.vt.is_held() {
                        // A synchronized-output hold freezes the frame; still
                        // deliver notifications without taking a redundant snapshot.
                        if self.drain_notifications() {
                            self.live.wake_for_notification();
                        }
                    } else {
                        self.publish(false);
                    }
                }
                NextWork::Control(Msg::Key(input)) => {
                    if self.vt.clear_selection() {
                        self.publish(false);
                    }
                    let bytes = self.vt.encode_input(&input);
                    self.write_pty(bytes);
                }
                NextWork::Control(Msg::Write(bytes)) => {
                    if self.vt.clear_selection() {
                        self.publish(false);
                    }
                    self.write_pty(bytes);
                }
                NextWork::Control(Msg::Paste(text)) => {
                    if self.vt.clear_selection() {
                        self.publish(false);
                    }
                    let bytes = self.vt.encode_paste(&text);
                    self.write_pty(bytes);
                }
                NextWork::Control(Msg::Mouse(input)) => {
                    let bytes = self.vt.encode_mouse(input);
                    self.write_pty(bytes);
                }
                NextWork::Control(Msg::Select {
                    step,
                    col,
                    row,
                    x,
                    y,
                    cell_w,
                    height,
                    reply,
                }) => {
                    self.vt.select(step, col, row, x, y, cell_w, height);
                    self.publish(false);
                    if let Some(reply) = reply {
                        let _ = reply.send(self.vt.selection_text());
                    }
                }
                NextWork::Control(Msg::ClearSelection) => {
                    if self.vt.clear_selection() {
                        self.publish(false);
                    }
                }
                NextWork::Control(Msg::SelectionText(reply)) => {
                    let _ = reply.send(self.vt.selection_text());
                }
                NextWork::Control(Msg::HyperlinkAt { col, row, reply }) => {
                    let _ = reply.send(self.vt.hyperlink_at(col, row));
                }
                NextWork::Control(Msg::SearchSet(needle)) => {
                    self.vt.search_set(&needle);
                    self.publish(false);
                }
                NextWork::Control(Msg::SearchStep { older }) => {
                    self.vt.search_step(older);
                    self.publish(false);
                }
                NextWork::Control(Msg::SearchClear) => {
                    if self.vt.search_clear() {
                        self.publish(false);
                    }
                }
                NextWork::Control(Msg::Content { history, reply }) => {
                    let _ = reply.send(Some(self.vt.content_text(history)));
                }
                NextWork::Control(Msg::Scroll {
                    lines,
                    x,
                    y,
                    cell_w,
                    cell_h,
                    mods,
                }) => {
                    if self.vt.mouse_tracking() {
                        let button = if lines < 0 {
                            super::vt::MouseButton::WheelUp
                        } else {
                            super::vt::MouseButton::WheelDown
                        };
                        let mut out = Vec::new();
                        for _ in 0..lines.unsigned_abs() {
                            out.extend(self.vt.encode_mouse(MouseInput {
                                action: super::vt::MouseAction::Press,
                                button: Some(button),
                                x,
                                y,
                                cell_w,
                                cell_h,
                                any_pressed: false,
                                mods,
                            }));
                        }
                        self.write_pty(out);
                    } else if self.vt.alt_screen() {
                        let bytes = self.vt.encode_scroll_keys(lines < 0, lines.unsigned_abs());
                        self.write_pty(bytes);
                    } else {
                        self.vt.scroll_viewport(lines);
                        self.publish(false);
                    }
                }
                NextWork::Control(Msg::Resize {
                    cols,
                    rows,
                    cell_w,
                    cell_h,
                }) => {
                    let _ = self.pty.master.resize(PtySize {
                        rows,
                        cols,
                        pixel_width: u16::try_from(cell_w)
                            .unwrap_or(u16::MAX)
                            .saturating_mul(cols),
                        pixel_height: u16::try_from(cell_h)
                            .unwrap_or(u16::MAX)
                            .saturating_mul(rows),
                    });
                    self.vt.resize(cols, rows, cell_w, cell_h);
                    self.flush_replies();
                    self.publish(false);
                }
                NextWork::Control(Msg::Colors(colors)) => {
                    self.vt.set_colors(colors);
                    self.publish(false);
                }
                NextWork::Eof => {
                    self.exited = true;
                    self.publish(true);
                    break;
                }
                NextWork::Shutdown => break,
            }
        }
    }

    fn flush_replies(&mut self) {
        let replies = self.vt.take_replies();
        self.write_pty(replies);
    }

    fn drain_notifications(&mut self) -> bool {
        let mut any = false;
        let mut pending = self
            .live
            .notifications
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        while let Some(notification) = self.vt.take_notification() {
            if pending.len() == MAX_NOTIFICATIONS {
                pending.pop_front();
            }
            pending.push_back(notification);
            any = true;
        }
        any
    }

    fn publish(&mut self, force_wake: bool) {
        let got_notification = self.drain_notifications();
        // A running find bar has to see new output and viewport scrolls; with
        // no needle this is a no-op.
        self.vt.search_feed();
        self.live.publish(
            self.vt.snapshot(&self.command, &self.cwd, self.exited),
            force_wake || got_notification,
        );
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    fn wait_text(term: &Term, needle: &str) -> String {
        let start = Instant::now();
        loop {
            let text = term.snapshot().plain_text();
            if text.contains(needle) {
                return text;
            }
            if start.elapsed() > Duration::from_secs(3) {
                return text;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn arbitrary_launch_commands_keep_arguments_and_inherit_only_terminal_identity() {
        let (term, _wake) = Term::launch(
            "/bin/sh".into(),
            vec![
                "-c".into(),
                r#"printf 'ARG=<%s>\n' "$@"; printf 'IDENTITY=%s\n' "$KUMU_TERMINAL"; printf 'PROGRAM=%s\n' "$TERM_PROGRAM"; printf 'TTY=%s\n' "$KUMU_TTY"; test -w "$KUMU_TTY" && echo tty-writable; echo identity-ready"#.into(),
                "custom-script".into(),
                "--settings".into(),
                "value with spaces".into(),
            ],
            std::env::temp_dir(),
            TermColors::default(),
        );
        let text = wait_text(&term, "identity-ready");
        assert!(text.contains("ARG=<--settings>"), "{text}");
        assert!(text.contains("ARG=<value with spaces>"), "{text}");
        assert!(text.contains("IDENTITY=1"), "{text}");
        assert!(text.contains("PROGRAM=kumu"), "{text}");
        assert!(text.contains("TTY=/dev/"), "{text}");
        assert!(text.contains("tty-writable"), "{text}");
    }

    #[test]
    fn pty_notifications_are_consumed_once_and_survive_screen_reads() {
        let (term, _wake) = Term::launch(
            "/bin/sh".into(),
            vec![
                "-c".into(),
                r"printf '\033]9;Task ready\007'; echo notification-sent; sleep 1".into(),
            ],
            std::env::temp_dir(),
            TermColors::default(),
        );
        assert!(wait_text(&term, "notification-sent").contains("notification-sent"));
        assert_eq!(
            term.take_notification().expect("PTY notification").body,
            "Task ready"
        );
        term.snapshot();
        term.resize(90, 30, 8, 16);
        assert_eq!(term.take_notification(), None);
    }

    #[test]
    fn notifications_from_one_read_are_consumed_in_order() {
        let (term, _wake) = Term::launch(
            "/bin/sh".into(),
            vec![
                "-c".into(),
                r"printf '\033]9;first\007\033]9;second\007'; echo notifications-sent; sleep 1"
                    .into(),
            ],
            std::env::temp_dir(),
            TermColors::default(),
        );
        assert!(wait_text(&term, "notifications-sent").contains("notifications-sent"));
        assert_eq!(term.take_notification().expect("first").body, "first");
        assert_eq!(term.take_notification().expect("second").body, "second");
        assert_eq!(term.take_notification(), None);
    }

    #[test]
    fn dropping_a_clone_leaves_the_process_running() {
        let (term, _wake) = Term::launch(
            "/bin/sh".into(),
            vec!["-c".into(), "sleep 0.3; echo still-alive".into()],
            std::env::temp_dir(),
            TermColors::default(),
        );
        drop(term.clone());
        let text = wait_text(&term, "still-alive");
        assert!(text.contains("still-alive"), "grid was:\n{text}");
    }

    #[test]
    fn echo_shows_up_on_the_grid() {
        let (term, _wake) = Term::launch(
            "/bin/echo".into(),
            vec!["hello-from-pty".into()],
            std::env::temp_dir(),
            TermColors::default(),
        );
        let text = wait_text(&term, "hello-from-pty");
        assert!(text.contains("hello-from-pty"), "grid was:\n{text}");
    }

    #[test]
    fn a_resize_is_what_the_process_measures() {
        let (term, _wake) = Term::launch(
            "/bin/sh".into(),
            vec!["-c".into(), "sleep 0.4; stty size".into()],
            std::env::temp_dir(),
            TermColors::default(),
        );
        term.resize(40, 12, 8, 16);
        let text = wait_text(&term, "12 40");
        assert!(text.contains("12 40"), "grid was:\n{text}");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn zsh_tab_completion_lists_command_candidates() {
        let (term, _wake) = Term::launch(
            "/bin/zsh".into(),
            vec!["-f".into(), "-i".into()],
            std::env::temp_dir(),
            TermColors::default(),
        );
        // A deterministic command-specific completer exercises the same ZLE
        // path as the user's z plugin, without loading their shell config.
        term.write(concat!(
            "autoload -Uz compinit; compinit -D; ",
            "unsetopt menucomplete; setopt autolist; ",
            "bindkey '^I' complete-word; ",
            "_kumu_z() { compadd -- kumu-alpha kumu-beta; }; ",
            "compdef _kumu_z z; PS1='kumu> '; ",
            "printf '\\nCOMPLETION_%s\\n' READY\n",
        ));
        let text = wait_text(&term, "COMPLETION_READY");
        assert!(text.contains("COMPLETION_READY"), "grid was:\n{text}");
        term.write(b"z kumu-");
        for _ in 0..2 {
            term.key(KeyInput {
                key: "tab".into(),
                key_char: Some("\t".into()),
                shift: false,
                control: false,
                alt: false,
            });
        }
        let text = wait_text(&term, "kumu-alpha  kumu-beta");
        assert!(
            text.lines()
                .any(|line| line.trim() == "kumu-alpha  kumu-beta"),
            "completion candidates were not listed:\n{text}"
        );
    }

    #[test]
    fn a_find_needle_highlights_matches_on_the_live_terminal() {
        let (term, _wake) = Term::launch(
            "/bin/sh".into(),
            vec![
                "-c".into(),
                "printf 'alpha\\nbeta\\nalpha\\n'; sleep 2".into(),
            ],
            std::env::temp_dir(),
            TermColors::default(),
        );
        assert!(wait_text(&term, "beta").contains("beta"));
        term.search_set("alpha");
        let start = Instant::now();
        let search = loop {
            if let Some(search) = term.snapshot().search
                && search.total == 2
            {
                break search;
            }
            assert!(
                start.elapsed() < Duration::from_secs(3),
                "find needle never reached the terminal"
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(search.selected, Some(0));
        assert!(!search.hits.is_empty());
        term.search_clear();
        let start = Instant::now();
        while term.snapshot().search.is_some() {
            assert!(
                start.elapsed() < Duration::from_secs(3),
                "clearing the needle left highlights"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn exiting_one_shell_leaves_the_other() {
        let (program, args) = crate::term::default_shell();
        let (a, _wa) = Term::launch(
            program.clone(),
            args.clone(),
            std::env::temp_dir(),
            TermColors::default(),
        );
        let (b, _wb) = Term::launch(program, args, std::env::temp_dir(), TermColors::default());
        std::thread::sleep(Duration::from_millis(200));
        assert!(!a.snapshot().exited);
        assert!(!b.snapshot().exited);
        a.write(b"exit\n");
        let start = Instant::now();
        while !a.snapshot().exited && start.elapsed() < Duration::from_secs(3) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(a.snapshot().exited, "A should have exited");
        drop(a);
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            !b.snapshot().exited,
            "B must stay alive; grid:\n{}",
            b.snapshot().plain_text()
        );
    }
}
