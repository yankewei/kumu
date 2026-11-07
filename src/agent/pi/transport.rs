//! The Pi RPC wire: newline-delimited JSON over the child's stdio.
//!
//! Not JSON-RPC and not ACP. There is no method field and no id the server
//! chooses: kumu stamps every command with its own string id, and Pi answers
//! with `{"type":"response","id":…,"success":…,"data":…}`. Everything else on
//! stdout is an unsolicited event, so a line is a reply if and only if its
//! `type` is `response` — which is why this module consumes replies here and
//! forwards everything else untouched.
//!
//! Two details of the framing matter:
//!
//!   - Pi's own documentation calls the format strict JSONL with LF as the only
//!     record delimiter, and warns that readers splitting on Unicode line
//!     separators are not protocol-compliant. `lines()` splits on `\n` and
//!     strips a trailing `\r`, and nothing else, so it is compliant.
//!   - A command is answered when it is *accepted*, not when the work it starts
//!     finishes. The turn that follows is followed on the event stream, not by
//!     waiting on the reply.

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};

/// How long a command may go unanswered before it is reported as failed.
///
/// Every command kumu sends is answered as soon as it is accepted — a prompt
/// acknowledgement does not wait for the model — so a reply that has not
/// arrived by now is not coming. Bounded rather than unlimited because the
/// alternative is a session parked behind a process that has stopped reading
/// its stdin.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// The replies we are still waiting for, keyed by the id we sent.
type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<Result<Value, String>>>>>;

/// The sending half of the connection, plus the table of in-flight commands.
pub(super) struct Transport {
    tx: mpsc::UnboundedSender<Value>,
    pending: Pending,
    next_id: AtomicU64,
}

impl Transport {
    /// Send a command and wait for Pi's reply.
    pub(super) async fn request(&self, request: Value) -> Result<Value, String> {
        let id = format!("waku-{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id.clone(), tx);

        let mut request = request;
        request["id"] = Value::String(id.clone());
        if self.tx.send(request).is_err() {
            self.pending.lock().unwrap().remove(&id);
            return Err("pi 不在运行".to_string());
        }

        match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(outcome)) => outcome,
            // The table is cleared when stdout closes, which drops this sender.
            Ok(Err(_)) => Err("pi 没有回应（进程已退出？）".to_string()),
            Err(_) => {
                // The late reply, if it ever arrives, finds no waiter and is
                // dropped by the reader.
                self.pending.lock().unwrap().remove(&id);
                Err("pi 响应超时".to_string())
            }
        }
    }
}

/// Launch Pi and start pumping its stdio.
///
/// Returns the transport, the inbound stream of events, and the child — which
/// the caller must keep, since dropping it kills the process.
///
/// `cwd` is the child's working directory, and for this backend it *is* the
/// session's working directory: Pi takes it from the process, not from a
/// parameter.
pub(super) fn spawn(
    command: &str,
    args: &[String],
    env: &[(String, String)],
    cwd: &Path,
) -> std::io::Result<(Transport, mpsc::UnboundedReceiver<Value>, Child)> {
    let mut child = Command::new(command)
        .args(args)
        .envs(env.iter().map(|(k, v)| (k.clone(), v.clone())))
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // Pi's own diagnostics are worth seeing; they inherit our stderr.
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()?;

    let stdin = child.stdin.take().expect("stdin is piped");
    let stdout = child.stdout.take().expect("stdout is piped");

    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Value>();
    let (frame_tx, frame_rx) = mpsc::unbounded_channel::<Value>();
    let pending: Pending = Arc::new(Mutex::new(HashMap::new()));

    // Writer: owns stdin, so no lock is ever held across an await.
    tokio::spawn(async move {
        let mut stdin = stdin;
        while let Some(message) = out_rx.recv().await {
            let mut line = message.to_string();
            line.push('\n');
            if stdin.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            let _ = stdin.flush().await;
        }
    });

    // Reader: fulfils our own commands and forwards everything else as an event.
    let reader_pending = pending.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let frame: Value = match serde_json::from_str(line) {
                Ok(frame) => frame,
                Err(error) => {
                    debug_log(&format!("unparsable line ({error})"));
                    continue;
                }
            };
            if frame.get("type").and_then(Value::as_str) == Some("response") {
                let Some(id) = frame.get("id").and_then(Value::as_str) else {
                    // A reply kumu cannot match to anything it sent. Pi only
                    // omits the id for commands it could not parse, so there is
                    // nothing to deliver it to.
                    debug_log("unmatched response");
                    continue;
                };
                let Some(waiter) = reader_pending.lock().unwrap().remove(id) else {
                    continue;
                };
                let outcome = if frame.get("success").and_then(Value::as_bool) == Some(true) {
                    Ok(frame)
                } else {
                    Err(frame
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("pi 拒绝了这条指令")
                        .to_string())
                };
                let _ = waiter.send(outcome);
                continue;
            }
            if frame_tx.send(frame).is_err() {
                break;
            }
        }
        // The pipe closed, so nothing else can be answered. Releasing the
        // waiters now is what keeps `request` from parking forever behind a Pi
        // that has already exited.
        reader_pending.lock().unwrap().clear();
        debug_log("stdout closed");
    });

    let transport = Transport {
        tx: out_tx,
        pending,
        next_id: AtomicU64::new(1),
    };
    Ok((transport, frame_rx, child))
}

/// Echo the wire traffic to stderr when KUMU_DEBUG is set, for headless
/// verification (mirrors the shared `emit` convention).
pub(super) fn debug_log(message: &str) {
    if std::env::var("KUMU_DEBUG").is_ok() {
        eprintln!("[pi] <- {message}");
    }
}
