//! The app-server wire: newline-delimited JSON over the child's stdio.
//!
//! Almost JSON-RPC, with one deviation that rules out an off-the-shelf client:
//! the server omits the `"jsonrpc": "2.0"` field on the wire, so a strict
//! JSON-RPC implementation rejects its messages. Shapes are told apart instead:
//!
//!   - `method` with no `id` — a notification;
//!   - `method` with an `id` — a server-initiated request, which **must** be
//!     answered or the turn parks forever;
//!   - `id` with `result`/`error` — the response to one of our requests.
//!
//! Request ids we generate are integers, but a server request's id can be a
//! string as well, so it travels as a raw [`Value`] and is echoed back verbatim.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};

/// A message the server sent that the session has to act on.
#[derive(Debug)]
pub(super) enum Message {
    /// No response expected.
    Notification { method: String, params: Value },
    /// A server-initiated request; answer it with [`Transport::respond`] or
    /// [`Transport::respond_error`].
    Request {
        id: Value,
        method: String,
        params: Value,
    },
}

/// The responses we are still waiting for, keyed by the id we sent.
type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, String>>>>>;

/// The sending half of the connection, plus the table of in-flight requests.
pub(super) struct Transport {
    tx: mpsc::UnboundedSender<Value>,
    pending: Pending,
    next_id: AtomicI64,
}

impl Transport {
    /// A transport with nowhere for its sends to go, for tests that drive the
    /// session's notification/request handlers directly and never expect a
    /// reply over the wire — mirrors the Claude backend's
    /// `Transport::detached_for_test` and `Connection::detached_for_test`.
    #[cfg(test)]
    pub(super) fn detached_for_test() -> Self {
        Self {
            tx: mpsc::unbounded_channel().0,
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: AtomicI64::new(0),
        }
    }

    /// Send a request and wait for its response.
    pub(super) async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let message = json!({ "id": id, "method": method, "params": params });
        if self.tx.send(message).is_err() {
            self.pending.lock().unwrap().remove(&id);
            return Err(format!("{method}: app-server is not running"));
        }
        match rx.await {
            Ok(outcome) => outcome,
            Err(_) => Err(format!("{method}: no response (app-server exited?)")),
        }
    }

    /// Send a notification; no response is expected.
    pub(super) fn notify(&self, method: &str, params: Value) {
        let _ = self.tx.send(json!({ "method": method, "params": params }));
    }

    /// Answer a server-initiated request.
    pub(super) fn respond(&self, id: Value, result: Value) {
        let _ = self.tx.send(json!({ "id": id, "result": result }));
    }

    /// Reject a server-initiated request.
    ///
    /// Used for the methods this backend does not implement: a protocol error
    /// is the honest answer, and unlike silence it lets the turn proceed or
    /// fail cleanly instead of waiting on a prompt nobody will ever answer.
    pub(super) fn respond_error(&self, id: Value, message: &str) {
        let _ = self.tx.send(json!({
            "id": id,
            "error": { "code": -32601, "message": message },
        }));
    }
}

/// Launch the agent and start pumping its stdio.
///
/// Returns the transport, the inbound stream of notifications and requests, and
/// the child — which the caller must keep, since dropping it kills the process.
/// This is what makes "drop the last `Connection`" a teardown for every
/// backend.
pub(super) fn spawn(
    command: &str,
    args: &[String],
    env: &[(String, String)],
) -> std::io::Result<(Transport, mpsc::UnboundedReceiver<Message>, Child)> {
    let mut child = Command::new(command)
        .args(args)
        .envs(env.iter().map(|(k, v)| (k.clone(), v.clone())))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // The agent's own diagnostics are worth seeing; they inherit our stderr.
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()?;

    let stdin = child.stdin.take().expect("stdin is piped");
    let stdout = child.stdout.take().expect("stdout is piped");

    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Value>();
    let (msg_tx, msg_rx) = mpsc::unbounded_channel::<Message>();
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

    // Reader: fulfils our own requests and forwards everything else.
    let reader_pending = pending.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let message: Value = match serde_json::from_str(line) {
                Ok(message) => message,
                Err(err) => {
                    debug_log(&format!("unparsable line ({err})"));
                    continue;
                }
            };
            if let Some(method) = message.get("method").and_then(Value::as_str) {
                debug_log(method);
                let params = message.get("params").cloned().unwrap_or(Value::Null);
                let incoming = match message.get("id").cloned() {
                    Some(id) => Message::Request {
                        id,
                        method: method.to_string(),
                        params,
                    },
                    None => Message::Notification {
                        method: method.to_string(),
                        params,
                    },
                };
                if msg_tx.send(incoming).is_err() {
                    break;
                }
            } else {
                let Some(id) = message.get("id").and_then(Value::as_i64) else {
                    continue;
                };
                let Some(waiter) = reader_pending.lock().unwrap().remove(&id) else {
                    continue;
                };
                let outcome = match message.get("error") {
                    Some(error) => Err(error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("app-server returned an error")
                        .to_string()),
                    None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                };
                let _ = waiter.send(outcome);
            }
        }
        // The pipe closed, so nothing else can be answered. Releasing the
        // waiters now is what keeps `request` from parking forever behind an
        // agent that has already exited.
        reader_pending.lock().unwrap().clear();
        debug_log("stdout closed");
    });

    let transport = Transport {
        tx: out_tx,
        pending,
        next_id: AtomicI64::new(1),
    };
    Ok((transport, msg_rx, child))
}

/// Echo the wire traffic to stderr when KUMU_DEBUG is set, for headless
/// verification (mirrors the shared `emit` convention).
pub(super) fn debug_log(message: &str) {
    if std::env::var("KUMU_DEBUG").is_ok() {
        eprintln!("[codex] <- {message}");
    }
}
