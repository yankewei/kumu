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

use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use futures::channel::mpsc::UnboundedSender;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// How long the server waits for the UI to answer one request. A split that
/// waits for the agent to draw its prompt is the slow one.
const REPLY_TIMEOUT: Duration = Duration::from_secs(15);
/// How often the accept loop looks at its stop flag.
const POLL: Duration = Duration::from_millis(100);
/// The most scrollback rows one read may ask for. Reading history pages the
/// viewport, so the bound is work, not storage.
pub const MAX_HISTORY: usize = 2_000;
/// The pid of the Kumu a pane's process belongs to. Kumu exports it to every
/// pane, so a client can tell its own app apart from another instance that
/// happens to serve the socket.
pub const PID_ENV: &str = "KUMU_APP_PID";

/// Where the socket lives. `KUMU_CONTROL_SOCKET` lets a test or a second
/// instance pick its own; the default sits beside the database.
pub fn socket_path() -> PathBuf {
    if let Some(path) = std::env::var_os("KUMU_CONTROL_SOCKET") {
        return PathBuf::from(path);
    }
    crate::db::db_path().with_file_name("control.sock")
}

// ---------------------------------------------------------------- protocol

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
}

fn submit_by_default() -> bool {
    true
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
    Split {
        direction: String,
        agent: String,
        from: Option<u64>,
        prompt: Option<String>,
        reply: Sender<Result<SplitResult, ControlError>>,
    },
    Send {
        target: PaneTarget,
        from: Option<u64>,
        text: String,
        enter: bool,
        reply: Sender<Result<SendResult, ControlError>>,
    },
}

// ------------------------------------------------------------------ server

/// Keeps the control socket alive. Dropping it stops the accept loop and
/// removes the socket, so a closed window leaves nothing for a client to
/// connect to.
pub struct ControlHandle {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for ControlHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Bind the socket and serve it from a thread of its own. Returns `None` when
/// another Kumu already owns the path, or when the directory cannot be made
/// private: a socket other users could open must not exist.
pub fn start(commands: UnboundedSender<Command>) -> Option<ControlHandle> {
    start_at(socket_path(), commands)
}

fn start_at(path: PathBuf, commands: UnboundedSender<Command>) -> Option<ControlHandle> {
    let dir = path.parent()?;
    std::fs::create_dir_all(dir).ok()?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).ok()?;
    // A socket file left by a crashed run is dead; one that still answers
    // belongs to another Kumu, which keeps the channel.
    if UnixStream::connect(&path).is_ok() {
        eprintln!("[control] {} is served by another Kumu", path.display());
        return None;
    }
    let _ = std::fs::remove_file(&path);
    let listener = match UnixListener::bind(&path) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("[control] cannot bind {}: {error}", path.display());
            return None;
        }
    };
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    if let Err(error) = listener.set_nonblocking(true) {
        eprintln!("[control] cannot watch {}: {error}", path.display());
        return None;
    }
    let stop = Arc::new(AtomicBool::new(false));
    let accepting = Arc::clone(&stop);
    let thread = std::thread::Builder::new()
        .name("kumu-control".into())
        .spawn(move || accept_loop(listener, commands, accepting))
        .ok()?;
    Some(ControlHandle {
        path,
        stop,
        thread: Some(thread),
    })
}

fn accept_loop(listener: UnixListener, commands: UnboundedSender<Command>, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => {
                let commands = commands.clone();
                // A client that connects and stalls must not keep another
                // client from being answered.
                std::thread::spawn(move || serve(stream, &commands));
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => std::thread::sleep(POLL),
            Err(_) => std::thread::sleep(POLL),
        }
    }
}

/// One connection, one request, one reply.
fn serve(stream: UnixStream, commands: &UnboundedSender<Command>) {
    let mut stream = stream;
    // The listener accepts in nonblocking mode, and macOS hands that flag to
    // the connection: left as is, a request that lands a moment after the
    // connection reads as WouldBlock and looks like an empty one. The
    // timeouts below only apply to a blocking socket.
    if stream.set_nonblocking(false).is_err() {
        return;
    }
    let read = stream.set_read_timeout(Some(REPLY_TIMEOUT));
    let write = stream.set_write_timeout(Some(REPLY_TIMEOUT));
    if read.is_err() || write.is_err() {
        return;
    }
    let mut line = String::new();
    // Read through a borrow: cloning the socket can fail when the process is
    // out of descriptors, which would read as an empty request too.
    let read = BufReader::new(&stream).read_line(&mut line).is_ok();
    let reply = if read {
        match serde_json::from_str::<RequestLine>(line.trim()) {
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
        }
    } else {
        Err(ControlError::new(
            "badRequest",
            "expected one JSON request line",
        ))
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
        Request::Split {
            direction,
            agent,
            from,
            prompt,
        } => exchange(commands, |reply| Command::Split {
            direction,
            agent,
            from,
            prompt,
            reply,
        }),
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
    }
}

/// One round trip through the UI: send the command, wait for its answer, and
/// turn the answer into the reply body.
fn exchange<T: Serialize>(
    commands: &UnboundedSender<Command>,
    command: impl FnOnce(Sender<Result<T, ControlError>>) -> Command,
) -> Result<Value, ControlError> {
    let (reply, answer) = mpsc::channel();
    commands
        .unbounded_send(command(reply))
        .map_err(|_| unavailable())?;
    match answer.recv_timeout(REPLY_TIMEOUT) {
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
        _ => None,
    }
}

const USAGE: &str = "\
usage:
  kumu panes [--json]
  kumu pane read [TARGET] [--history N] [--tail N] [--json]
  kumu pane split right|down --agent <id> [--prompt TEXT] [--json]
  kumu pane send <TARGET> <TEXT...> [--no-enter] [--json]

TARGET is a pane id, left/right/up/down (relative to $KUMU_PANE), or focused
(the default). --history N reads N scrollback rows; --tail N keeps the last N
lines of the answer. A split starts an agent in the new pane; its --prompt is
sent once that agent has drawn its own prompt. In send, -- makes everything
after it text, so it may start with a dash.";

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
        println!("{}", value["panes"]);
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
        Some("split") => pane_split(&args[1..]),
        Some("send") => pane_send(&args[1..]),
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
            "--" => text_only = true,
            "--json" => json = true,
            "--no-enter" => enter = false,
            "-h" | "--help" => {
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
    stream
        .write_all(format!("{body}\n").as_bytes())
        .and_then(|()| stream.flush())
        .map_err(|error| format!("cannot send the request: {error}"))?;
    // The server gives up on a busy app after REPLY_TIMEOUT; wait a little past
    // that so its error reaches the user instead of a silent hang.
    let _ = stream.set_read_timeout(Some(REPLY_TIMEOUT + Duration::from_secs(2)));
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .map_err(|error| format!("kumu did not answer: {error}"))?;
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
}
