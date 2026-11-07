//! Newline-delimited JSON over the child's stdio, for the settings probe.
//!
//! Not JSON-RPC and not ACP. The probe sends one `control_request` (`initialize`)
//! and waits for the `control_response` with the same `request_id`. Every other
//! line is ignored: a probe never starts a turn, so there is no stream to
//! render and no CLI request to answer.
//!
//! See `schema/claude-stream-json/2.1.269/README.md`.

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};

use super::protocol::ControlResponse;

/// The control responses we are still waiting for, keyed by the id we sent.
type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<Result<Value, String>>>>>;

/// The sending half of the connection, plus the table of in-flight requests.
pub(super) struct Transport {
    tx: mpsc::UnboundedSender<Value>,
    pending: Pending,
    next_id: AtomicU64,
}

impl Transport {
    /// Send a control request and wait for the CLI's `control_response`.
    pub(super) async fn control(&self, request: Value) -> Result<Value, String> {
        let request_id = request_id(self.next_id.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(request_id.clone(), tx);
        let message = json!({
            "type": "control_request",
            "request_id": request_id,
            "request": request,
        });
        if self.tx.send(message).is_err() {
            self.pending.lock().unwrap().remove(&request_id);
            return Err("claude 不在运行".to_string());
        }
        match rx.await {
            Ok(Ok(payload)) => Ok(payload),
            Ok(Err(error)) => Err(error),
            Err(_) => Err("claude 没有回应控制请求（进程已退出？）".to_string()),
        }
    }
}

/// Launch the agent and start pumping its stdio.
///
/// Returns the transport and the child — which the caller must keep, since
/// dropping it kills the process.
///
/// `cwd` is the child's working directory, and for this backend it *is* the
/// session's working directory: unlike ACP and Codex, which name it in their
/// session/thread parameters, Claude Code takes it from the process.
pub(super) fn spawn(
    command: &str,
    args: &[String],
    env: &[(String, String)],
    cwd: &Path,
) -> std::io::Result<(Transport, Child)> {
    let mut child = Command::new(command)
        .args(args)
        .envs(env.iter().map(|(k, v)| (k.clone(), v.clone())))
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // The CLI's own diagnostics are worth seeing; they inherit our stderr.
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()?;

    let stdin = child.stdin.take().expect("stdin is piped");
    let stdout = child.stdout.take().expect("stdout is piped");

    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Value>();
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

    // Reader: fulfils our own control requests and drops everything else.
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
            match message.get("type").and_then(Value::as_str).unwrap_or("") {
                "control_response" => {
                    // The frame is `{"type":"control_response","response":{…}}`,
                    // and the inner object is the response itself.
                    let response: ControlResponse =
                        serde_json::from_value(message["response"].clone()).unwrap_or_default();
                    let Some(waiter) = reader_pending.lock().unwrap().remove(&response.request_id)
                    else {
                        continue;
                    };
                    let outcome = if response.subtype == "error" {
                        Err(response
                            .error
                            .unwrap_or_else(|| "claude 返回了错误".to_string()))
                    } else {
                        Ok(response.response.unwrap_or(Value::Null))
                    };
                    let _ = waiter.send(outcome);
                }
                other => debug_log(other),
            }
        }
        // The pipe closed, so nothing else can be answered. Releasing the
        // waiters now is what keeps `control` from parking forever behind a CLI
        // that has already exited.
        reader_pending.lock().unwrap().clear();
        debug_log("stdout closed");
    });

    let transport = Transport {
        tx: out_tx,
        pending,
        next_id: AtomicU64::new(1),
    };
    Ok((transport, child))
}

/// A request id shaped like the ones the Agent SDK sends.
///
/// The CLI treats these as opaque and only echoes them back, but matching the
/// SDK's shape costs nothing and avoids betting on it accepting anything else.
/// Uniqueness comes from the process id, the clock and a counter, so two ids
/// cannot collide within a run.
fn request_id(sequence: u64) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as u64)
        .unwrap_or(0);
    let mut x =
        nanos ^ sequence.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ ((std::process::id() as u64) << 32);
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    let mut y = x ^ 0xD1B5_4A32_D192_ED03;
    y ^= y << 13;
    y ^= y >> 7;
    y ^= y << 17;
    format!(
        "{:08x}-{:04x}-4{:03x}-8{:03x}-{:012x}",
        (x >> 32) as u32,
        (x >> 16) as u16,
        (x & 0x0fff) as u16,
        (y >> 48) as u16 & 0x0fff,
        y & 0xffff_ffff_ffff
    )
}

/// Echo the wire traffic to stderr when KUMU_DEBUG is set, for headless
/// verification (mirrors the shared `emit` convention).
pub(super) fn debug_log(message: &str) {
    if std::env::var("KUMU_DEBUG").is_ok() {
        eprintln!("[claude] <- {message}");
    }
}
