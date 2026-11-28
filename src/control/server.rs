use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use futures::channel::mpsc::UnboundedSender;
use serde::Serialize;
use serde_json::{Value, json};

use super::protocol::{Command, ControlError, MAX_WAIT_TIMEOUT_SECS, Request, RequestLine};
use super::{
    MAX_CONCURRENT_CLIENTS, MAX_HISTORY, MAX_REQUEST_BYTES, MAX_SCROLL_LINES, POLL, REPLY_TIMEOUT,
    socket_path,
};

unsafe extern "C" {
    fn geteuid() -> u32;
    fn flock(fd: i32, operation: i32) -> i32;
    #[cfg(target_os = "macos")]
    fn getpeereid(fd: i32, euid: *mut u32, egid: *mut u32) -> i32;
}

const LOCK_EX: i32 = 2;
const LOCK_NB: i32 = 4;

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

pub(super) fn start_at(path: PathBuf, commands: UnboundedSender<Command>) -> Option<ControlHandle> {
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
        Request::ArtifactPublished {
            from,
            project_path,
            artifact,
        } => exchange(commands, |reply| Command::ArtifactPublished {
            from,
            project_path,
            artifact,
            reply,
        }),
        Request::ArtifactOpen {
            from,
            project_path,
            artifact,
        } => exchange(commands, |reply| Command::ArtifactOpen {
            from,
            project_path,
            artifact,
            reply,
        }),
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
