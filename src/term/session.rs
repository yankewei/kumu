//! The thread that owns one PTY and its terminal emulator.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::channel::mpsc as futures_mpsc;
use futures::channel::mpsc::UnboundedReceiver;
use portable_pty::PtySize;

use super::pty::{self, Pty};
use super::vt::{
    KeyInput, MAX_NOTIFICATIONS, MouseInput, MouseMods, SelectStep, Snapshot, TermColors,
    TermNotification, Vt,
};

struct Live {
    screen: Mutex<Snapshot>,
    notifications: Mutex<VecDeque<TermNotification>>,
    generation: AtomicU64,
    queued: AtomicBool,
    /// The PTY child (the shell), or 0 before it is spawned. Lets the app
    /// inspect what the user started inside it, e.g. an agent that should not
    /// outlive the choice to resume it.
    pid: AtomicU64,
    wake: futures::channel::mpsc::UnboundedSender<()>,
    /// Remote viewers of the raw PTY stream. A viewer that cannot keep up is
    /// dropped instead of stalling the terminal.
    taps: Mutex<HashMap<u64, futures_mpsc::Sender<Vec<u8>>>>,
}

/// Tap ids are unique across every terminal in the process. A pane whose
/// terminal is replaced must not hand a stale id to a viewer of the new one.
static NEXT_TAP: AtomicU64 = AtomicU64::new(1);

impl Live {
    fn publish(&self, snapshot: Snapshot) {
        *self.screen.lock().unwrap() = snapshot;
        self.generation.fetch_add(1, Ordering::Release);
        if !self.queued.swap(true, Ordering::AcqRel) {
            let _ = self.wake.unbounded_send(());
        }
    }

    fn forward(&self, bytes: &[u8]) {
        let mut taps = self.taps.lock().unwrap();
        taps.retain(|_, tap| tap.try_send(bytes.to_vec()).is_ok());
    }
}

/// One remote viewer: a screen replay plus the live bytes that follow it.
pub struct RemoteAttach {
    pub tap: u64,
    pub cols: u16,
    pub rows: u16,
    pub replay: Vec<u8>,
    pub output: futures_mpsc::Receiver<Vec<u8>>,
}

enum Msg {
    Bytes(Vec<u8>),
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
    /// Find bar: set the needle (empty clears), or walk matches.
    SearchSet(String),
    SearchStep {
        older: bool,
    },
    SearchClear,
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
    /// A remote viewer joins at this grid size. The reply carries the replay
    /// and the live stream; the session thread registers the tap before it
    /// takes the snapshot, so no bytes fall between the two.
    ///
    /// `attach` orders concurrent attempts: the newest one wins the grid even
    /// when an older background attach reaches this thread last.
    Attach {
        attach: u64,
        cols: u16,
        rows: u16,
        reply: Sender<Option<RemoteAttach>>,
    },
    DetachTap(u64),
    Eof,
    Shutdown,
}

/// Sends shutdown when the last [`Term`] handle is dropped.
struct Close {
    tx: Sender<Msg>,
}

impl Drop for Close {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Shutdown);
    }
}

/// Handle to a running terminal. Dropping the last handle stops the process.
#[derive(Clone)]
pub struct Term {
    tx: Sender<Msg>,
    live: Arc<Live>,
    /// Keeps the process alive until the last handle is dropped.
    _close: Arc<Close>,
}

impl Term {
    /// Start `program` in `cwd` on a new PTY. The receiver wakes when the grid changes.
    pub fn launch(
        program: String,
        args: Vec<String>,
        cwd: PathBuf,
        colors: TermColors,
    ) -> (Self, UnboundedReceiver<()>) {
        let (tx, rx) = mpsc::channel();
        let (wake, wake_rx) = futures::channel::mpsc::unbounded();
        let command = std::path::Path::new(&program)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or(program.clone());
        let cwd_text = cwd.display().to_string();
        let live = Arc::new(Live {
            screen: Mutex::new(Snapshot::blank_with(&command, &cwd_text, colors.bg)),
            notifications: Mutex::new(VecDeque::new()),
            generation: AtomicU64::new(0),
            queued: AtomicBool::new(false),
            pid: AtomicU64::new(0),
            wake,
            taps: Mutex::new(HashMap::new()),
        });
        let thread_tx = tx.clone();
        let thread_live = Arc::clone(&live);
        let launch = Launch {
            program,
            args,
            cwd,
            command,
            cwd_text,
            colors,
        };
        std::thread::Builder::new()
            .name("kumu-term".into())
            .spawn(move || run(launch, thread_tx, rx, thread_live))
            .expect("spawn terminal thread");
        let _close = Arc::new(Close { tx: tx.clone() });
        (Self { tx, live, _close }, wake_rx)
    }

    pub fn snapshot(&self) -> Snapshot {
        self.live.screen.lock().unwrap().clone()
    }

    /// Consume attention separately from the screen so redraws cannot replay it.
    pub fn take_notification(&self) -> Option<TermNotification> {
        self.live
            .notifications
            .lock()
            .expect("terminal notification lock")
            .pop_front()
    }

    pub fn generation(&self) -> u64 {
        self.live.generation.load(Ordering::Acquire)
    }

    /// The pid of the PTY child, once it has spawned.
    pub fn child_pid(&self) -> Option<u32> {
        let pid = self.live.pid.load(Ordering::Acquire);
        (pid != 0).then_some(pid as u32)
    }

    /// The UI has started handling a wake. A publish that arrives after this
    /// sends another one.
    pub fn wake_handled(&self) {
        self.live.queued.store(false, Ordering::Release);
    }

    pub fn key(&self, input: KeyInput) {
        let _ = self.tx.send(Msg::Key(input));
    }

    /// Write raw bytes to the PTY (finished IME text).
    pub fn write(&self, bytes: impl Into<Vec<u8>>) {
        let bytes = bytes.into();
        if !bytes.is_empty() {
            let _ = self.tx.send(Msg::Write(bytes));
        }
    }

    /// Paste clipboard text, framed for the program's bracketed paste mode.
    pub fn paste(&self, text: impl Into<Vec<u8>>) {
        let text = text.into();
        if !text.is_empty() {
            let _ = self.tx.send(Msg::Paste(text));
        }
    }

    pub fn mouse(&self, input: MouseInput) {
        let _ = self.tx.send(Msg::Mouse(input));
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
        let _ = self.tx.send(Msg::Select {
            step,
            col,
            row,
            x,
            y,
            cell_w,
            height,
            reply,
        });
        wait.and_then(|rx| rx.recv().ok()).flatten()
    }

    pub fn clear_selection(&self) {
        let _ = self.tx.send(Msg::ClearSelection);
    }

    /// Search the terminal for `needle` and select the newest match. An empty
    /// needle closes the find bar's highlights.
    pub fn search_set(&self, needle: impl Into<String>) {
        let _ = self.tx.send(Msg::SearchSet(needle.into()));
    }

    /// Move the selected match toward older (`older`) or newer content.
    pub fn search_step(&self, older: bool) {
        let _ = self.tx.send(Msg::SearchStep { older });
    }

    pub fn search_clear(&self) {
        let _ = self.tx.send(Msg::SearchClear);
    }

    /// Currently selected plain text, if any.
    ///
    /// Blocks until the session thread answers. Callers run on the background
    /// executor, and a busy session thread (a paste, a flood of output) must not
    /// turn ⌘C into a silent no-op: a short timeout here just drops the copy.
    /// The channel disconnecting is the one thing that ends the wait early,
    /// which is the session thread being gone.
    pub fn selection_text(&self) -> Option<String> {
        let (tx, rx) = mpsc::channel();
        let _ = self.tx.send(Msg::SelectionText(tx));
        rx.recv().ok().flatten()
    }

    pub fn scroll(&self, lines: isize, x: f32, y: f32, cell_w: f32, cell_h: f32, mods: MouseMods) {
        if lines == 0 {
            return;
        }
        let _ = self.tx.send(Msg::Scroll {
            lines,
            x,
            y,
            cell_w,
            cell_h,
            mods,
        });
    }

    /// Attach a remote viewer at `cols` × `rows`.
    ///
    /// Blocks until the session thread has resized, registered the tap and
    /// produced the replay. Call this off the UI thread.
    ///
    /// `attach` orders concurrent requests: a request older than one this
    /// terminal already applied is refused, so the last attach to reach the
    /// thread cannot leave a newer viewer on the wrong grid.
    pub fn attach_remote(&self, attach: u64, cols: u16, rows: u16) -> Option<RemoteAttach> {
        let (reply, wait) = mpsc::channel();
        let _ = self.tx.send(Msg::Attach {
            attach,
            cols,
            rows,
            reply,
        });
        wait.recv_timeout(Duration::from_secs(2)).ok().flatten()
    }

    pub fn detach_remote(&self, tap: u64) {
        let _ = self.tx.send(Msg::DetachTap(tap));
    }

    pub fn resize(&self, cols: u16, rows: u16, cell_w: u32, cell_h: u32) {
        let _ = self.tx.send(Msg::Resize {
            cols,
            rows,
            cell_w,
            cell_h,
        });
    }

    pub fn set_colors(&self, colors: TermColors) {
        let _ = self.tx.send(Msg::Colors(colors));
    }
}

/// What a terminal thread needs at start: the command, where it runs, and the
/// labels its snapshots carry.
struct Launch {
    program: String,
    args: Vec<String>,
    cwd: PathBuf,
    command: String,
    cwd_text: String,
    colors: TermColors,
}

fn run(launch: Launch, tx: Sender<Msg>, rx: Receiver<Msg>, live: Arc<Live>) {
    let Launch {
        program,
        args,
        cwd,
        command,
        cwd_text,
        colors,
    } = launch;
    let size = PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 80 * 8,
        pixel_height: 24 * 16,
    };
    let pty = match pty::open(&program, &args, &cwd, size) {
        Ok(pty) => pty,
        Err(error) => {
            let mut snapshot = Snapshot::blank_with(&command, &cwd_text, colors.bg);
            snapshot.exited = true;
            snapshot.spawn_error = Some(error.to_string());
            live.publish(snapshot);
            return;
        }
    };
    if let Some(pid) = pty.child.process_id() {
        live.pid.store(pid as u64, Ordering::Release);
    }
    let mut session = Running {
        pty,
        vt: Vt::with_colors(80, 24, colors),
        command,
        cwd: cwd_text,
        exited: false,
        live,
        last_attach: 0,
    };
    session.vt.resize(80, 24, 8, 16);
    session.publish();

    let reader_tx = tx;
    // The reader is moved into the read thread, so take it out of the pty first.
    let mut reader = std::mem::replace(&mut session.pty.reader, Box::new(std::io::empty()));
    std::thread::Builder::new()
        .name("kumu-pty".into())
        .spawn(move || read_pty(&mut reader, &reader_tx))
        .expect("spawn pty reader");

    session.pump(rx);
    let _ = session.pty.child.kill();
    let _ = session.pty.child.wait();
}

fn read_pty(reader: &mut dyn Read, tx: &Sender<Msg>) {
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => {
                let _ = tx.send(Msg::Eof);
                break;
            }
            Ok(n) => {
                if tx.send(Msg::Bytes(buf[..n].to_vec())).is_err() {
                    break;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => {
                let _ = tx.send(Msg::Eof);
                break;
            }
        }
    }
}

struct Running {
    pty: Pty,
    vt: Vt,
    command: String,
    cwd: String,
    exited: bool,
    live: Arc<Live>,
    /// The newest remote attach this terminal has applied — see [`Msg::Attach`].
    last_attach: u64,
}

impl Running {
    fn pump(&mut self, rx: Receiver<Msg>) {
        loop {
            let msg = match self.vt.hold_wait() {
                Some(wait) => match rx.recv_timeout(wait) {
                    Ok(msg) => Some(msg),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => break,
                },
                None => match rx.recv() {
                    Ok(msg) => Some(msg),
                    Err(_) => break,
                },
            };
            match msg {
                None => self.publish(),
                Some(Msg::Bytes(bytes)) => {
                    self.vt.write(&bytes);
                    self.flush_replies();
                    self.live.forward(&bytes);
                    self.publish();
                }
                Some(Msg::Key(input)) => {
                    if self.vt.clear_selection() {
                        self.publish();
                    }
                    let bytes = self.vt.encode_input(&input);
                    if !bytes.is_empty() {
                        let _ = self.pty.writer.write_all(&bytes);
                        let _ = self.pty.writer.flush();
                    }
                }
                Some(Msg::Write(bytes)) => {
                    if self.vt.clear_selection() {
                        self.publish();
                    }
                    let _ = self.pty.writer.write_all(&bytes);
                    let _ = self.pty.writer.flush();
                }
                Some(Msg::Paste(text)) => {
                    if self.vt.clear_selection() {
                        self.publish();
                    }
                    let bytes = self.vt.encode_paste(&text);
                    if !bytes.is_empty() {
                        let _ = self.pty.writer.write_all(&bytes);
                        let _ = self.pty.writer.flush();
                    }
                }
                Some(Msg::Mouse(input)) => {
                    let bytes = self.vt.encode_mouse(input);
                    if !bytes.is_empty() {
                        let _ = self.pty.writer.write_all(&bytes);
                        let _ = self.pty.writer.flush();
                    }
                }
                Some(Msg::Select {
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
                    self.publish();
                    if let Some(reply) = reply {
                        let _ = reply.send(self.vt.selection_text());
                    }
                }
                Some(Msg::ClearSelection) => {
                    if self.vt.clear_selection() {
                        self.publish();
                    }
                }
                Some(Msg::SelectionText(reply)) => {
                    let _ = reply.send(self.vt.selection_text());
                }
                Some(Msg::SearchSet(needle)) => {
                    self.vt.search_set(&needle);
                    self.publish();
                }
                Some(Msg::SearchStep { older }) => {
                    self.vt.search_step(older);
                    self.publish();
                }
                Some(Msg::SearchClear) => {
                    if self.vt.search_clear() {
                        self.publish();
                    }
                }
                Some(Msg::Scroll {
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
                        if !out.is_empty() {
                            let _ = self.pty.writer.write_all(&out);
                            let _ = self.pty.writer.flush();
                        }
                    } else if self.vt.alt_screen() {
                        let bytes = self.vt.encode_scroll_keys(lines < 0, lines.unsigned_abs());
                        if !bytes.is_empty() {
                            let _ = self.pty.writer.write_all(&bytes);
                            let _ = self.pty.writer.flush();
                        }
                    } else {
                        self.vt.scroll_viewport(lines);
                        self.publish();
                    }
                }
                Some(Msg::Resize {
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
                    self.publish();
                }
                Some(Msg::Colors(colors)) => {
                    self.vt.set_colors(colors);
                    self.publish();
                }
                Some(Msg::Attach {
                    attach,
                    cols,
                    rows,
                    reply,
                }) => {
                    if attach < self.last_attach {
                        // A newer attach already took the grid. This attempt
                        // lost the race to reach the session thread; its lease
                        // was superseded, so it must not resize the terminal.
                        let _ = reply.send(None);
                        continue;
                    }
                    self.last_attach = attach;
                    let cols = cols.max(1);
                    let rows = rows.max(1);
                    let _ = self.pty.master.resize(PtySize {
                        rows,
                        cols,
                        pixel_width: 8 * cols,
                        pixel_height: 16 * rows,
                    });
                    self.vt.resize(cols, rows, 8, 16);
                    self.flush_replies();
                    self.publish();
                    let (tap_tx, output) = futures_mpsc::channel(256);
                    let tap = NEXT_TAP.fetch_add(1, Ordering::AcqRel);
                    self.live.taps.lock().unwrap().insert(tap, tap_tx);
                    // The replay is the live screen, not wherever the Mac left
                    // the viewport. Put the viewport back afterwards so the
                    // desktop's scroll position is undisturbed.
                    let restore = self.vt.scrollbar().and_then(|bar| {
                        let bottom = bar.total.saturating_sub(bar.len);
                        (bar.offset < bottom).then(|| {
                            self.vt.scroll_to_row(bottom as usize);
                            bar.offset as usize
                        })
                    });
                    let snapshot = self.vt.snapshot(&self.command, &self.cwd, self.exited);
                    if let Some(row) = restore {
                        self.vt.scroll_to_row(row);
                    }
                    let replay =
                        snapshot.ansi_replay(self.vt.alt_screen(), self.vt.bracketed_paste());
                    let _ = reply.send(Some(RemoteAttach {
                        tap,
                        cols,
                        rows,
                        replay,
                        output,
                    }));
                }
                Some(Msg::DetachTap(tap)) => {
                    self.live.taps.lock().unwrap().remove(&tap);
                }
                Some(Msg::Eof) => {
                    self.exited = true;
                    self.publish();
                    break;
                }
                Some(Msg::Shutdown) => break,
            }
        }
    }

    fn flush_replies(&mut self) {
        let replies = self.vt.take_replies();
        if replies.is_empty() {
            return;
        }
        let _ = self.pty.writer.write_all(&replies);
        let _ = self.pty.writer.flush();
    }

    fn publish(&mut self) {
        let mut pending = self
            .live
            .notifications
            .lock()
            .expect("terminal notification lock");
        while let Some(notification) = self.vt.take_notification() {
            if pending.len() == MAX_NOTIFICATIONS {
                pending.pop_front();
            }
            pending.push_back(notification);
        }
        drop(pending);
        // A running find bar has to see new output and viewport scrolls; with
        // no needle this is a no-op.
        self.vt.search_feed();
        self.live
            .publish(self.vt.snapshot(&self.command, &self.cwd, self.exited));
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
