use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

use super::cli_pane::{pane_cli, panes_cli};
use super::protocol::{MAX_WAIT_TIMEOUT_SECS, PaneTarget, Request, RequestLine};
use super::{MAX_RESPONSE_BYTES, PID_ENV, REPLY_TIMEOUT, socket_path};

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
        "artifact" => Some(super::cli_artifact::artifact_cli(&rest)),
        "integration" => Some(super::cli_integration::integration_cli(&rest)),
        "mcp" => Some(super::mcp::mcp_cli(&rest)),
        _ => None,
    }
}

pub(super) const USAGE: &str = "\
usage:
  kumu panes [--json]
  kumu pane read [TARGET] [--history N] [--tail N] [--json]
  kumu pane scroll up|down [TARGET] [--lines N] [--col C] [--row R] [--json]
  kumu pane split right|down --agent <id> [--prompt TEXT] [--json]
  kumu pane send <TARGET> <TEXT...> [--no-enter] [--json]
  kumu pane wait <TARGET> [--timeout SECS] [--next] [--json]
  kumu pane close <TARGET> [--self] [--json]
  kumu artifact <publish|list|path|open|remove> ...   (see kumu artifact --help)
  kumu integration <install|uninstall|status> ...     (see kumu integration --help)
  kumu mcp [serve|install|uninstall|status] ...       (see kumu mcp --help)

TARGET is a pane id, left/right/up/down (relative to $KUMU_PANE), or focused
(the default). --history N reads N scrollback rows; --tail N keeps the last N
lines of the answer. A split starts an agent in the new pane; its --prompt is
sent once that agent has drawn its own prompt. In send, -- makes everything
after it text, so it may start with a dash. wait blocks until the target agent
enters waiting/completed/failed or exits (--next waits for the next transition).";

pub(super) fn usage_error(message: &str) -> i32 {
    eprintln!("kumu: {message}\n{USAGE}");
    2
}

pub(super) fn runtime_error(message: &str) -> i32 {
    eprintln!("kumu: {message}");
    1
}

pub(super) fn print_json(value: &impl Serialize) -> i32 {
    match serde_json::to_string(value) {
        Ok(text) => {
            println!("{text}");
            0
        }
        Err(error) => runtime_error(&error.to_string()),
    }
}

/// The caller's own pane id: a direction is relative to it.
pub(super) fn caller_pane() -> Option<u64> {
    std::env::var("KUMU_PANE")
        .ok()
        .and_then(|value| value.parse().ok())
}

/// A numeric argument is a pane id; anything else is read as a name.
pub(super) fn pane_target(target: Option<String>) -> PaneTarget {
    match target.as_deref().map(str::parse::<u64>) {
        Some(Ok(id)) => PaneTarget::Id(id),
        _ => PaneTarget::Name(target.unwrap_or_else(|| "focused".into())),
    }
}

pub(super) fn flags(args: &[String]) -> Result<(bool, Vec<String>), i32> {
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
pub(super) fn call(request: &Request) -> Result<Value, String> {
    call_at_instance(&socket_path(), request, caller_instance())
}

/// The pid of the Kumu this pane belongs to, exported as [`PID_ENV`]. A reply
/// from another pid means another Kumu instance serves the socket.
fn caller_instance() -> Option<u32> {
    std::env::var(PID_ENV)
        .ok()
        .and_then(|value| value.parse().ok())
}

pub(super) fn call_at_instance(
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

/// The last `count` lines of `text`; `count == 0` keeps nothing.
pub(super) fn last_lines(text: &str, count: usize) -> String {
    if count == 0 {
        return String::new();
    }
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(count)..].join("\n")
}
