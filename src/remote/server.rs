//! Authenticated control and terminal frames on one reliable iroh stream.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context, Result};
use futures::StreamExt;
use futures::channel::mpsc::UnboundedSender;
use iroh::Endpoint;
use iroh::endpoint::Connection;
use serde_json::{Value, json};
use tokio::sync::{Notify, Semaphore, mpsc, oneshot};
use tokio::task::JoinHandle;

use super::auth::{Auth, Device};
use super::protocol::{
    KIND_CLOSE, KIND_CREDIT, KIND_DATA, KIND_REPLAY, KIND_RESIZE, MAX_BINARY, Message, PROTOCOL,
    frame, read_message, write_message,
};
use super::state::{PaneStatus, RemoteState};
use super::{Action, Command, MAX_GRID, RemoteError};

const AUTH_TIMEOUT: Duration = Duration::from_secs(15);
const ATTACH_TIMEOUT: Duration = Duration::from_secs(10);
const ACTION_TIMEOUT: Duration = Duration::from_secs(10);
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a control reply may wait for room behind terminal data before it
/// is dropped: only a phone that stopped reading gets here.
const CONTROL_SEND_TIMEOUT: Duration = Duration::from_secs(5);
const INITIAL_CREDIT: u64 = 256 * 1024;

pub(crate) struct RemoteServer {
    pub endpoint: Endpoint,
    pub name: String,
    pub auth: Arc<Auth>,
    pub state: Arc<RwLock<RemoteState>>,
    pub commands: UnboundedSender<Command>,
    pub max_connections: usize,
}

impl RemoteServer {
    pub async fn serve(self: Arc<Self>) -> Result<()> {
        let connections = Arc::new(Semaphore::new(self.max_connections.max(1)));
        while let Some(incoming) = self.endpoint.accept().await {
            let Ok(permit) = connections.clone().try_acquire_owned() else {
                incoming.refuse();
                continue;
            };
            let server = self.clone();
            tokio::spawn(async move {
                let _permit = permit;
                match tokio::time::timeout(Duration::from_secs(10), incoming).await {
                    Ok(Ok(connection)) => {
                        let peer = connection.remote_id().to_string();
                        if let Err(error) = server.session(connection.clone()).await {
                            eprintln!("[remote] {peer}: {error:#}");
                        }
                        connection.close(0u32.into(), b"session ended");
                    }
                    other => eprintln!("[remote] handshake failed: {other:?}"),
                }
            });
        }
        Ok(())
    }

    async fn session(&self, connection: Connection) -> Result<()> {
        let peer = connection.remote_id().to_string();
        let (mut sink, mut source) =
            tokio::time::timeout(Duration::from_secs(15), connection.accept_bi())
                .await
                .context("session stream timed out")??;

        let (out_tx, mut out_rx) = mpsc::channel::<Message>(64);
        let writer_connection = connection.clone();
        let mut writer = tokio::spawn(async move {
            while let Some(message) = out_rx.recv().await {
                if write_message(&mut sink, &message).await.is_err() {
                    writer_connection.close(0u32.into(), b"writer failed");
                    return;
                }
            }
            let _ = sink.finish();
            let _ = tokio::time::timeout(Duration::from_secs(1), sink.stopped()).await;
        });

        // `read_message` is not cancellation-safe once a header is partly read,
        // so it gets a task of its own instead of sitting in `select!`.
        let (in_tx, mut in_rx) = mpsc::channel(1);
        let reader = tokio::spawn(async move {
            loop {
                match read_message(&mut source).await {
                    Ok(Some(message)) => {
                        if in_tx.send(Ok(message)).await.is_err() {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        let _ = in_tx.send(Err(error)).await;
                        break;
                    }
                }
            }
        });

        let mut authenticated: Option<Device> = None;
        let mut channels: HashMap<u32, Channel> = HashMap::new();
        let mut next_channel = 1u32;
        let mut revocations = self.auth.subscribe_revocations();
        let deadline = tokio::time::Instant::now() + AUTH_TIMEOUT;
        let mut last_seen = tokio::time::Instant::now();

        loop {
            tokio::select! {
                _ = &mut writer => break,
                _ = connection.closed() => break,
                _ = tokio::time::sleep_until(last_seen + HEARTBEAT_TIMEOUT), if authenticated.is_some() => {
                    break;
                }
                revoked = revocations.recv(), if authenticated.is_some() => {
                    match revoked {
                        Ok(id) => {
                            if authenticated.as_ref().is_some_and(|device| device.id == id) {
                                send_control(&out_tx, rpc_error(None, "unauthorized", "device revoked")).await;
                                break;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            if authenticated.as_ref().is_some_and(|device| !self.auth.is_current(&device.id)) {
                                send_control(&out_tx, rpc_error(None, "unauthorized", "device revoked")).await;
                                break;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
                _ = tokio::time::sleep_until(deadline), if authenticated.is_none() => {
                    send_control(&out_tx, rpc_error(None, "unauthorized", "authentication timed out")).await;
                    break;
                }
                incoming = in_rx.recv() => {
                    let Some(incoming) = incoming else { break };
                    last_seen = tokio::time::Instant::now();
                    let message = match incoming {
                        Ok(message) => message,
                        Err(error) => {
                            eprintln!("[remote] {peer}: stream closed: {error}");
                            break;
                        }
                    };
                    match message {
                        Message::Text(text) => {
                            if !self.handle_control(
                                &text,
                                &peer,
                                &out_tx,
                                &mut authenticated,
                                &mut channels,
                                &mut next_channel,
                            ).await {
                                break;
                            }
                        }
                        Message::Binary(bytes) => {
                            if authenticated.is_some() {
                                self.handle_terminal(&bytes, &out_tx, &mut channels);
                            }
                        }
                    }
                }
            }
        }

        for (_, channel) in channels {
            channel.task.abort();
            let _ = self.commands.unbounded_send(Command::Detach {
                pane: channel.pane,
                tap: channel.tap,
                reply: None,
            });
        }
        reader.abort();
        writer.abort();
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_control(
        &self,
        text: &str,
        peer: &str,
        out_tx: &mpsc::Sender<Message>,
        authenticated: &mut Option<Device>,
        channels: &mut HashMap<u32, Channel>,
        next_channel: &mut u32,
    ) -> bool {
        let Ok(request) = serde_json::from_str::<Value>(text) else {
            send_control(out_tx, rpc_error(None, "invalid_params", "invalid JSON")).await;
            return true;
        };
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let method = request["method"].as_str().unwrap_or_default();
        let params = request.get("params").cloned().unwrap_or(Value::Null);

        match method {
            "hello" => {
                if params["protocol"].as_u64() != Some(PROTOCOL) {
                    send_control(
                        out_tx,
                        rpc_error(
                            Some(id),
                            "protocol_mismatch",
                            "unsupported protocol; update Kumu Mobile",
                        ),
                    )
                    .await;
                    return false;
                }
                match params.get("token").and_then(Value::as_str) {
                    Some(token) => match self.auth.authenticate(token, peer) {
                        Some(device) => {
                            *authenticated = Some(device.clone());
                            send_control(
                                out_tx,
                                ok(
                                    id,
                                    json!({
                                        "bridge": self.bridge_info(),
                                        "device": {"id": device.id, "name": device.name},
                                        "protocol": PROTOCOL,
                                    }),
                                ),
                            )
                            .await;
                        }
                        None => {
                            send_control(
                                out_tx,
                                rpc_error(Some(id), "unauthorized", "unknown or revoked device"),
                            )
                            .await
                        }
                    },
                    None => {
                        send_control(
                            out_tx,
                            ok(
                                id,
                                json!({
                                    "needsPairing": true,
                                    "bridge": self.bridge_info(),
                                    "protocol": PROTOCOL,
                                }),
                            ),
                        )
                        .await
                    }
                }
                true
            }
            "pair" => {
                if authenticated.is_some() {
                    send_control(
                        out_tx,
                        rpc_error(Some(id), "invalid_state", "already paired"),
                    )
                    .await;
                    return true;
                }
                let code = params["code"].as_str().unwrap_or_default();
                let name = params["name"].as_str().unwrap_or("iPhone");
                match self.auth.pair(code, name, peer) {
                    Ok(paired) => {
                        *authenticated = Some(paired.device.clone());
                        send_control(
                            out_tx,
                            ok(
                                id,
                                json!({
                                    "token": paired.token,
                                    "device": {
                                        "id": paired.device.id,
                                        "name": paired.device.name,
                                    },
                                    "bridge": self.bridge_info(),
                                    "protocol": PROTOCOL,
                                }),
                            ),
                        )
                        .await;
                    }
                    Err(error) => {
                        send_control(
                            out_tx,
                            rpc_error(Some(id), "pairing_failed", &error.to_string()),
                        )
                        .await
                    }
                }
                true
            }
            "ping" => {
                send_control(out_tx, ok(id, json!({"pong": true}))).await;
                true
            }
            _ => {
                if authenticated.is_none() {
                    send_control(
                        out_tx,
                        rpc_error(Some(id), "unauthorized", "pair or authenticate first"),
                    )
                    .await;
                    return true;
                }
                match method {
                    "workspace.state" => {
                        let state = self.state.read().expect("remote state poisoned").clone();
                        send_control(
                            out_tx,
                            ok(id, serde_json::to_value(state).unwrap_or(Value::Null)),
                        )
                        .await;
                    }
                    "pane.attach" => {
                        let pane = params["pane"].as_u64().unwrap_or(0);
                        let cols = params["cols"]
                            .as_u64()
                            .unwrap_or(80)
                            .clamp(1, MAX_GRID as u64) as u16;
                        let rows = params["rows"]
                            .as_u64()
                            .unwrap_or(24)
                            .clamp(1, MAX_GRID as u64) as u16;
                        // A reconnect or a new phone takes the pane over. The
                        // old tap is released first so the desktop restores its
                        // size before the new grid is applied.
                        let existing = channels
                            .iter()
                            .find(|(_, channel)| channel.pane == pane)
                            .map(|(id, channel)| (*id, channel.tap));
                        if let Some((old_id, old_tap)) = existing
                            && let Some(old) = channels.remove(&old_id)
                        {
                            old.task.abort();
                            let _ = self.commands.unbounded_send(Command::Detach {
                                pane,
                                tap: old_tap,
                                reply: None,
                            });
                        }
                        let channel = *next_channel;
                        *next_channel = next_channel.wrapping_add(1).max(1);
                        match self.attach(pane, cols, rows).await {
                            Ok(attached) => {
                                // One frame must carry the whole replay. Keep
                                // the pane and the connection alive when a
                                // very large screen would not fit.
                                if attached.replay.len() + 9 > MAX_BINARY {
                                    let _ = self.commands.unbounded_send(Command::Detach {
                                        pane,
                                        tap: attached.tap,
                                        reply: None,
                                    });
                                    send_control(
                                        out_tx,
                                        rpc_error(
                                            Some(id),
                                            "invalid_state",
                                            "the terminal screen is too large to replay",
                                        ),
                                    )
                                    .await;
                                    return true;
                                }
                                let credit = Arc::new(Credit::new(INITIAL_CREDIT));
                                let response = ok(
                                    id,
                                    json!({
                                        "channel": channel,
                                        "cols": attached.cols,
                                        "rows": attached.rows,
                                        "credit": INITIAL_CREDIT,
                                        "title": attached.title,
                                        "status": attached.status,
                                        "lease": "control",
                                    }),
                                );
                                if out_tx
                                    .send(Message::Text(response.to_string()))
                                    .await
                                    .is_err()
                                {
                                    let _ = self.commands.unbounded_send(Command::Detach {
                                        pane,
                                        tap: attached.tap,
                                        reply: None,
                                    });
                                    return false;
                                }
                                if out_tx
                                    .send(Message::Binary(frame(
                                        channel,
                                        KIND_REPLAY,
                                        &attached.replay,
                                    )))
                                    .await
                                    .is_err()
                                {
                                    let _ = self.commands.unbounded_send(Command::Detach {
                                        pane,
                                        tap: attached.tap,
                                        reply: None,
                                    });
                                    return false;
                                }
                                let task = spawn_pump(
                                    channel,
                                    attached.output,
                                    credit.clone(),
                                    out_tx.clone(),
                                );
                                channels.insert(
                                    channel,
                                    Channel {
                                        pane,
                                        tap: attached.tap,
                                        credit,
                                        task,
                                    },
                                );
                            }
                            Err(error) => {
                                send_control(
                                    out_tx,
                                    rpc_error(Some(id), &error.kind, &error.message),
                                )
                                .await
                            }
                        }
                    }
                    "pane.detach" => {
                        let channel = params["channel"].as_u64().unwrap_or(0) as u32;
                        if let Some(channel) = channels.remove(&channel) {
                            channel.task.abort();
                            let _ = self.commands.unbounded_send(Command::Detach {
                                pane: channel.pane,
                                tap: channel.tap,
                                reply: None,
                            });
                        }
                        send_control(out_tx, ok(id, json!({}))).await;
                    }
                    "workspace.action" => {
                        let action: Action = match serde_json::from_value(params) {
                            Ok(action) => action,
                            Err(_) => {
                                send_control(
                                    out_tx,
                                    rpc_error(Some(id), "invalid_params", "unknown action"),
                                )
                                .await;
                                return true;
                            }
                        };
                        let (reply, wait) = oneshot::channel();
                        if self
                            .commands
                            .unbounded_send(Command::Action { action, reply })
                            .is_err()
                        {
                            send_control(
                                out_tx,
                                rpc_error(Some(id), "invalid_state", "desktop is not available"),
                            )
                            .await;
                            return true;
                        }
                        match tokio::time::timeout(ACTION_TIMEOUT, wait).await {
                            Ok(Ok(Ok(()))) => {
                                send_control(out_tx, ok(id, json!({"ok": true}))).await
                            }
                            Ok(Ok(Err(error))) => {
                                send_control(
                                    out_tx,
                                    rpc_error(Some(id), &error.kind, &error.message),
                                )
                                .await
                            }
                            Ok(Err(_)) => {
                                send_control(
                                    out_tx,
                                    rpc_error(
                                        Some(id),
                                        "invalid_state",
                                        "desktop closed the request",
                                    ),
                                )
                                .await
                            }
                            Err(_) => {
                                send_control(
                                    out_tx,
                                    rpc_error(Some(id), "invalid_state", "desktop did not answer"),
                                )
                                .await
                            }
                        }
                    }
                    _ => {
                        send_control(
                            out_tx,
                            rpc_error(Some(id), "invalid_params", "unknown method"),
                        )
                        .await
                    }
                }
                true
            }
        }
    }

    fn handle_terminal(
        &self,
        bytes: &[u8],
        out_tx: &mpsc::Sender<Message>,
        channels: &mut HashMap<u32, Channel>,
    ) {
        if bytes.len() < 9 {
            return;
        }
        let channel_id = u32::from_le_bytes(bytes[..4].try_into().expect("four bytes"));
        let kind = bytes[4];
        let length = u32::from_le_bytes(bytes[5..9].try_into().expect("four bytes")) as usize;
        if bytes.len() != 9 + length {
            return;
        }
        let Some(channel) = channels.get(&channel_id) else {
            return;
        };
        let (pane, tap) = (channel.pane, channel.tap);
        let credit = channel.credit.clone();
        let payload = &bytes[9..];
        match kind {
            KIND_DATA => {
                let _ = self.commands.unbounded_send(Command::Input {
                    pane,
                    tap,
                    bytes: payload.to_vec(),
                });
            }
            KIND_CREDIT => {
                if payload.len() == 4 {
                    let granted =
                        u32::from_le_bytes(payload.try_into().expect("four bytes")) as u64;
                    credit.grant(granted);
                }
            }
            KIND_RESIZE => {
                if payload.len() == 4 {
                    let cols = u16::from_le_bytes(payload[..2].try_into().expect("two bytes"));
                    let rows = u16::from_le_bytes(payload[2..].try_into().expect("two bytes"));
                    let _ = self.commands.unbounded_send(Command::Resize {
                        pane,
                        tap,
                        cols,
                        rows,
                    });
                }
            }
            KIND_CLOSE => {
                // The phone gave the channel up: drop the lease and stop
                // streaming, exactly as `pane.detach` would.
                if let Some(channel) = channels.remove(&channel_id) {
                    channel.task.abort();
                    let _ = self.commands.unbounded_send(Command::Detach {
                        pane,
                        tap,
                        reply: None,
                    });
                    let _ = out_tx.try_send(Message::Binary(frame(
                        channel_id,
                        KIND_CLOSE,
                        br#"{"reason":"detached"}"#,
                    )));
                }
            }
            _ => {}
        }
    }

    async fn attach(&self, pane: u64, cols: u16, rows: u16) -> Result<Attached, RemoteError> {
        let (reply, wait) = oneshot::channel();
        self.commands
            .unbounded_send(Command::Attach {
                pane,
                cols,
                rows,
                reply,
            })
            .map_err(|_| RemoteError::new("invalid_state", "desktop is not available"))?;
        let handle = tokio::time::timeout(ATTACH_TIMEOUT, wait)
            .await
            .map_err(|_| RemoteError::new("invalid_state", "desktop did not answer"))?
            .map_err(|_| RemoteError::new("invalid_state", "desktop closed the request"))??;
        Ok(Attached {
            tap: handle.tap,
            cols: handle.cols,
            rows: handle.rows,
            title: handle.title,
            status: handle.status,
            replay: handle.replay,
            output: handle.output,
        })
    }

    fn bridge_info(&self) -> Value {
        json!({
            "name": self.name,
            "id": self.auth.bridge_id(),
            "endpointId": self.endpoint.id().to_string(),
            "protocol": PROTOCOL,
        })
    }
}

struct Attached {
    tap: u64,
    cols: u16,
    rows: u16,
    title: String,
    status: PaneStatus,
    replay: Vec<u8>,
    output: futures::channel::mpsc::Receiver<Vec<u8>>,
}

fn spawn_pump(
    channel: u32,
    mut output: futures::channel::mpsc::Receiver<Vec<u8>>,
    credit: Arc<Credit>,
    out: mpsc::Sender<Message>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(bytes) = output.next().await {
            if bytes.is_empty() {
                continue;
            }
            credit.acquire(bytes.len() as u64).await;
            if out
                .send(Message::Binary(frame(channel, KIND_DATA, &bytes)))
                .await
                .is_err()
            {
                return;
            }
        }
        let _ = out
            .send(Message::Binary(frame(
                channel,
                KIND_CLOSE,
                br#"{"reason":"terminal closed"}"#,
            )))
            .await;
    })
}

struct Channel {
    pane: u64,
    tap: u64,
    credit: Arc<Credit>,
    task: JoinHandle<()>,
}

struct Credit {
    available: AtomicU64,
    notify: Notify,
}

impl Credit {
    fn new(initial: u64) -> Self {
        Self {
            available: AtomicU64::new(initial),
            notify: Notify::new(),
        }
    }

    fn grant(&self, bytes: u64) {
        self.available.fetch_add(bytes, Ordering::Release);
        self.notify.notify_waiters();
    }

    async fn acquire(&self, bytes: u64) {
        loop {
            // Register this waiter before re-reading the counter: a grant that
            // lands between the read and the await would otherwise be lost,
            // because `notify_waiters` does not store a permit.
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let available = self.available.load(Ordering::Acquire);
            if available >= bytes {
                if self
                    .available
                    .compare_exchange(
                        available,
                        available - bytes,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
                {
                    return;
                }
                continue;
            }
            notified.await;
        }
    }
}

/// Queue a control frame and wait for room. Awaiting instead of dropping a
/// reply keeps a poll or a detach from stalling on a request that never
/// answers while terminal data is in flight; the timeout keeps a phone that
/// stopped reading from wedging the session.
async fn send_control(out_tx: &mpsc::Sender<Message>, value: Value) {
    let message = Message::Text(value.to_string());
    let _ = tokio::time::timeout(CONTROL_SEND_TIMEOUT, out_tx.send(message)).await;
}

fn ok(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn rpc_error(id: Option<Value>, kind: &str, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id.unwrap_or(Value::Null),
        "error": {
            "code": -32000,
            "message": message,
            "data": {"kind": kind},
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::protocol::{ALPN, PairingLink};
    use crate::remote::{AttachHandle, RelayChoice, RemoteConfig};
    use iroh::Endpoint;
    use iroh::endpoint::presets;
    use serde_json::json;
    use std::time::Duration;

    fn temp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kumu-remote-server-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    async fn read_text(recv: &mut iroh::endpoint::RecvStream) -> Value {
        let message = tokio::time::timeout(Duration::from_secs(10), read_message(recv))
            .await
            .expect("message timed out")
            .expect("read failed")
            .expect("stream ended");
        match message {
            Message::Text(text) => serde_json::from_str(&text).expect("invalid JSON"),
            Message::Binary(_) => panic!("expected control text"),
        }
    }

    async fn read_binary(recv: &mut iroh::endpoint::RecvStream) -> Vec<u8> {
        let message = tokio::time::timeout(Duration::from_secs(10), read_message(recv))
            .await
            .expect("message timed out")
            .expect("read failed")
            .expect("stream ended");
        match message {
            Message::Binary(bytes) => bytes,
            Message::Text(text) => panic!("expected binary frame, got {text}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pairing_state_attach_and_input_round_trip() {
        let dir = temp_dir();
        let state = Arc::new(RwLock::new(RemoteState {
            computer: crate::remote::ComputerInfo {
                name: "Test Mac".into(),
                id: "bridge".into(),
            },
            version: 7,
            ..RemoteState::default()
        }));
        let (cmd_tx, mut cmd_rx) = futures::channel::mpsc::unbounded();
        let config = RemoteConfig {
            name: "Test Mac".into(),
            state_dir: dir.clone(),
            relay: RelayChoice::Disabled,
            discovery: false,
            max_connections: 2,
        };
        // `start` owns a runtime and blocks while it binds the endpoint, so it
        // must not run on the test's async runtime.
        let handle =
            tokio::task::spawn_blocking(move || crate::remote::start(config, state, cmd_tx))
                .await
                .expect("start task")
                .expect("start remote");
        let link = PairingLink::parse(&handle.pairing_link().expect("pairing link"))
            .expect("parse pairing link");
        let code = handle.auth.code();

        let actor = tokio::spawn(async move {
            let mut input = Vec::new();
            let mut detached = false;
            while let Some(command) = cmd_rx.next().await {
                match command {
                    crate::remote::Command::Attach {
                        cols, rows, reply, ..
                    } => {
                        let (mut tx, output) = futures::channel::mpsc::channel(8);
                        tx.try_send(b"LIVE".to_vec()).expect("queue live bytes");
                        let _ = reply.send(Ok(AttachHandle {
                            tap: 7,
                            cols,
                            rows,
                            title: "smoke".into(),
                            status: PaneStatus::Idle,
                            replay: b"REPLAY".to_vec(),
                            output,
                        }));
                    }
                    crate::remote::Command::Input { bytes, .. } => input.extend(bytes),
                    crate::remote::Command::Detach { .. } => {
                        detached = true;
                        break;
                    }
                    _ => {}
                }
            }
            (input, detached)
        });

        let client = Endpoint::builder(presets::Minimal)
            .bind()
            .await
            .expect("client endpoint");
        let connection = client
            .connect(link.address.clone(), ALPN)
            .await
            .expect("connect");
        let (mut send, mut recv) = connection.open_bi().await.expect("open stream");

        write_message(
            &mut send,
            &Message::Text(
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "hello",
                    "params": {"protocol": PROTOCOL, "deviceName": "test"}
                })
                .to_string(),
            ),
        )
        .await
        .unwrap();
        let hello = read_text(&mut recv).await;
        assert_eq!(hello["result"]["needsPairing"], true);

        write_message(
            &mut send,
            &Message::Text(
                json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "pair",
                    "params": {"code": code, "name": "test phone"}
                })
                .to_string(),
            ),
        )
        .await
        .unwrap();
        let paired = read_text(&mut recv).await;
        let token = paired["result"]["token"]
            .as_str()
            .expect("token")
            .to_owned();

        write_message(
            &mut send,
            &Message::Text(
                json!({
                    "jsonrpc": "2.0",
                    "id": 3,
                    "method": "workspace.state"
                })
                .to_string(),
            ),
        )
        .await
        .unwrap();
        let workspace = read_text(&mut recv).await;
        assert_eq!(workspace["result"]["version"], 7);

        write_message(
            &mut send,
            &Message::Text(
                json!({
                    "jsonrpc": "2.0",
                    "id": 4,
                    "method": "pane.attach",
                    "params": {"pane": 1, "cols": 40, "rows": 20}
                })
                .to_string(),
            ),
        )
        .await
        .unwrap();
        let attached = read_text(&mut recv).await;
        let channel = attached["result"]["channel"].as_u64().expect("channel") as u32;
        assert_eq!(attached["result"]["cols"], 40);
        assert!(!token.is_empty());
        assert_eq!(&read_binary(&mut recv).await[9..], b"REPLAY");
        assert_eq!(&read_binary(&mut recv).await[9..], b"LIVE");

        write_message(
            &mut send,
            &Message::Binary(frame(channel, KIND_DATA, b"ls\n")),
        )
        .await
        .unwrap();
        write_message(
            &mut send,
            &Message::Text(
                json!({
                    "jsonrpc": "2.0",
                    "id": 5,
                    "method": "pane.detach",
                    "params": {"channel": channel}
                })
                .to_string(),
            ),
        )
        .await
        .unwrap();
        let (input, detached) = tokio::time::timeout(Duration::from_secs(10), actor)
            .await
            .expect("actor timed out")
            .expect("actor failed");
        assert_eq!(input, b"ls\n");
        assert!(detached);

        drop(connection);
        drop(handle);
        client.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}
