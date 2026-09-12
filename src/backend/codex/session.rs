//! The Codex session: handshake, one thread, and the command loop.
//!
//! Deliberately shaped like the ACP backend — one session per connection,
//! serial prompts, approvals answered out of band through the shared permission
//! table — so which agent is talking stays invisible to the UI.
//!
//! One difference in kind: app-server multiplexes threads over a single
//! process, but kumu still runs one process per session. That is what keeps
//! "dropping the last `Connection` reclaims the agent" true.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use crate::backend::config::Config;
use crate::backend::connection::WeakPermissions;
use crate::backend::event::{Cmd, Event, ModelInfo, emit};

use super::convert;
use super::protocol::{
    AccountResult, CommandApprovalParams, DeltaNotification, ErrorNotification,
    FileChangeApprovalParams, InitializeResult, ItemNotification, ModelListResult,
    ThreadStartResult, Turn, TurnCompletedNotification, TurnStartResult,
};
use super::transport::{Message, Transport};

/// State the message handler and the command loop both need.
struct Shared {
    /// The wire, so a server request can be answered from either task.
    transport: Arc<Transport>,
    ev_tx: Sender<Event>,
    permissions: WeakPermissions,
    /// Names our permission prompts; the wire's request id belongs to the agent.
    next_request_id: AtomicU64,
    /// Fires when the in-flight turn ends, carrying the turn's final state, so
    /// the command loop can stay serial exactly like the ACP backend's
    /// prompt-and-wait.
    turn_done: Mutex<Option<oneshot::Sender<Turn>>>,
    /// Command output accumulated per item. The UI *replaces* a row's content
    /// rather than appending, so deltas have to be summed before being sent up.
    output: Mutex<HashMap<String, String>>,
    /// Rendered content per item, so an approval prompt can show the diff it
    /// refers to — the request itself carries only the item id.
    details: Mutex<HashMap<String, String>>,
}

/// The prompt an approval request turns into.
struct ApprovalPrompt {
    item_id: String,
    title: String,
    detail: String,
}

/// The session's picks as a turn carries them: everything `turn/start` accepts
/// as a per-turn override. Grouped so `run_turn` keeps a readable argument list.
#[derive(Clone, Copy)]
struct TurnOverrides<'a> {
    model: Option<&'a str>,
    sandbox: Option<&'a str>,
    reasoning: Option<&'a str>,
}

/// The session's catalogue and picks, so the options event can be rebuilt
/// whenever a pick changes what is on offer.
///
/// This exists for one reason: Codex advertises reasoning efforts per model, so
/// switching model changes the list, and the UI cannot derive the new one. The
/// per-model shape is reduced here to the session-level list the boundary
/// carries (ACP's shape), and re-reported on every model change.
struct SessionState {
    models: Vec<ModelInfo>,
    /// Each model's reasoning axis, keyed by model value. Backend-internal.
    efforts: HashMap<String, convert::ModelReasoning>,
    /// The model in force.
    model: Option<String>,
    /// The user's picks; `None` leaves the agent's own configuration in charge.
    /// The reasoning pick is kept even while an incompatible model is in force,
    /// so switching back restores it.
    sandbox: Option<String>,
    pick: Option<String>,
    /// What the thread reported is in force, for the chip before the user picks.
    /// It is never sent back: re-asserting a resolved policy could change it
    /// (Codex's own `workspaceWrite`, for example, carries roots the picker's
    /// bare form does not).
    reported_sandbox: Option<String>,
    /// The effort a turn carries: the pick reconciled against the model in force.
    reasoning: Option<String>,
}

impl SessionState {
    /// The effort to carry: the one wanted when the model in force advertises
    /// it, else that model's own default. `None` for a model with no reasoning
    /// axis, which is what keeps the chip — and the `effort` field — absent.
    fn reconcile(&self, wanted: Option<&str>) -> Option<String> {
        if convert::supports_effort(&self.efforts, self.model.as_deref(), wanted) {
            return wanted.map(str::to_string);
        }
        convert::default_effort(&self.efforts, self.model.as_deref())
    }

    /// Report the session's options: the catalogue, and what is in force on each
    /// axis. Sent again whenever the model changes, because the reasoning set is
    /// the model's.
    fn emit(&self, agent: &'static str, ev_tx: &Sender<Event>) {
        emit(
            ev_tx,
            Event::SessionOptions {
                agent,
                models: self.models.clone(),
                current_model: self.model.clone(),
                modes: Vec::new(),
                current_mode: None,
                sandboxes: convert::sandbox_options(),
                current_sandbox: self
                    .sandbox
                    .clone()
                    .or_else(|| self.reported_sandbox.clone()),
                reasoning: convert::reasoning_options(&self.efforts, self.model.as_deref()),
                current_reasoning: self.reasoning.clone(),
            },
        );
    }
}

pub(in crate::backend) async fn run(
    cfg: Config,
    cwd: PathBuf,
    ev_tx: Sender<Event>,
    mut cmd_rx: mpsc::UnboundedReceiver<Cmd>,
    permissions: WeakPermissions,
) -> Result<(), String> {
    emit(
        &ev_tx,
        Event::Log(format!("spawning: {} {}", cfg.command, cfg.args.join(" "))),
    );
    let (transport, messages, _child) = super::transport::spawn(&cfg.command, &cfg.args, &cfg.env)
        .map_err(|err| format!("failed to launch {}: {err}", cfg.command))?;
    let transport = Arc::new(transport);

    let shared = Arc::new(Shared {
        transport: transport.clone(),
        ev_tx: ev_tx.clone(),
        permissions,
        next_request_id: AtomicU64::new(1),
        turn_done: Mutex::new(None),
        output: Mutex::new(HashMap::new()),
        details: Mutex::new(HashMap::new()),
    });
    // Notifications and approvals are handled on their own task, so the command
    // loop below is never blocked by them.
    tokio::spawn(handle_messages(shared.clone(), messages));

    // `initialize` is required before anything else on a connection.
    let initialized: InitializeResult =
        request(&transport, "initialize", super::client_info()).await?;
    transport.notify("initialized", json!({}));
    emit(
        &ev_tx,
        Event::Log(format!(
            "initialized: agent={} home={}",
            initialized.user_agent, initialized.codex_home
        )),
    );

    // Codex owns its credentials, and kumu deliberately adds no login UI: say
    // plainly what is missing instead of letting every turn fail upstream.
    let account: AccountResult = request(&transport, "account/read", json!({})).await?;
    if account.requires_openai_auth && account.account.is_none() {
        emit(
            &ev_tx,
            Event::Error(
                "Codex 未登录：请先在终端运行 `codex login`，然后重新打开这个会话。".to_string(),
            ),
        );
        return Ok(());
    }

    // The UI pushes a session's remembered picks onto the connection the moment
    // it opens one, so by now they are almost always queued behind the
    // handshake. Collecting them here rather than in the loop below lets them
    // configure the thread itself — which is both the honest place for a
    // session-wide setting and the only one the agent echoes back, so the
    // picker can label itself from the real policy instead of assuming one.
    // Anything else (a prompt) keeps its order and is handled by the loop.
    let mut queue: VecDeque<Cmd> = VecDeque::new();
    while let Ok(cmd) = cmd_rx.try_recv() {
        queue.push_back(cmd);
    }
    let mut model: Option<String> = None;
    let mut sandbox: Option<String> = None;
    let mut reasoning: Option<String> = None;
    queue.retain(|cmd| match cmd {
        Cmd::SetModel(value) => {
            model = Some(value.clone());
            false
        }
        Cmd::SetSandbox(value) => {
            sandbox = Some(value.clone());
            false
        }
        // Absorbed here only so it keeps its place ahead of any prompt, not to
        // configure the thread: `thread/start` takes no effort, so a reasoning
        // pick can only ever be applied per turn (see `run_turn`).
        Cmd::SetReasoning(value) => {
            reasoning = Some(value.clone());
            false
        }
        Cmd::SetMode(value) => {
            // Codex exposes no session mode this backend maps yet; the mode
            // chip stays hidden because no modes are ever reported.
            emit(
                &ev_tx,
                Event::Log(format!("no mode config option to set: {value}")),
            );
            false
        }
        Cmd::Prompt(_) => true,
    });

    // One thread per session, matching kumu's session tree. Only what the user
    // actually chose is sent: no approval policy is forced (the user's own Codex
    // configuration decides when a prompt is needed, and kumu surfaces whatever
    // arrives), and an unset sandbox stays unset so the agent's configuration
    // remains in charge.
    let mut params = json!({ "cwd": cwd });
    if let Some(model) = &model {
        params["model"] = json!(model);
    }
    if let Some(sandbox) = &sandbox {
        // `thread/start` takes the kebab-case string; `turn/start` wants the
        // tagged object. `convert` owns that asymmetry.
        params["sandbox"] = json!(sandbox);
    }
    let started: ThreadStartResult = request(&transport, "thread/start", params).await?;
    let thread_id = started.thread.id.clone();
    emit(
        &ev_tx,
        Event::Log(format!(
            "session ready: {thread_id} (provider: {})",
            started.model_provider
        )),
    );

    // The catalogues. An empty mode list is correct: the picker renders nothing
    // when the agent reports no modes.
    let listed: ModelListResult = request(&transport, "model/list", json!({})).await?;
    let (catalog_model, models) = convert::model_catalog(&listed);
    // The thread's own report is the truth about what is in force; the catalogue
    // only knows what the agent recommends.
    let current_model = if started.model.is_empty() {
        catalog_model
    } else {
        Some(started.model)
    };
    let mut state = SessionState {
        models,
        efforts: convert::model_reasoning(&listed),
        model: current_model,
        sandbox,
        pick: reasoning,
        reported_sandbox: started
            .sandbox
            .as_ref()
            .and_then(convert::sandbox_mode_value)
            .map(str::to_string),
        reasoning: None,
    };
    // The user's pick wins; else the effort the thread reports for itself. Either
    // way it only stands if the model in force advertises it.
    let wanted = state
        .pick
        .clone()
        .or_else(|| started.reasoning_effort.clone());
    state.reasoning = state.reconcile(wanted.as_deref());
    state.emit(cfg.agent, &ev_tx);

    // Headless convenience, matching the ACP backend: send one prompt before
    // the UI loop starts.
    if let Some(auto) = cfg.auto_prompt.clone() {
        emit(&ev_tx, Event::Log(format!("auto prompt: {auto}")));
        run_turn(
            &transport,
            &shared,
            &ev_tx,
            &thread_id,
            TurnOverrides {
                model: state.model.as_deref(),
                sandbox: state.sandbox.as_deref(),
                reasoning: state.reasoning.as_deref(),
            },
            &auto,
        )
        .await;
    }

    loop {
        let cmd = match queue.pop_front() {
            Some(cmd) => cmd,
            None => match cmd_rx.recv().await {
                Some(cmd) => cmd,
                None => break,
            },
        };
        match cmd {
            Cmd::Prompt(text) => {
                run_turn(
                    &transport,
                    &shared,
                    &ev_tx,
                    &thread_id,
                    TurnOverrides {
                        model: state.model.as_deref(),
                        sandbox: state.sandbox.as_deref(),
                        reasoning: state.reasoning.as_deref(),
                    },
                    &text,
                )
                .await;
            }
            Cmd::SetModel(value) => {
                emit(&ev_tx, Event::Log(format!("model set: {value}")));
                state.model = Some(value);
                // The reasoning set belongs to the model, so a pick this model
                // does not advertise stops being sent — and the picker has to be
                // told the new set, which it cannot derive itself.
                let wanted = state.pick.clone();
                state.reasoning = state.reconcile(wanted.as_deref());
                state.emit(cfg.agent, &ev_tx);
            }
            Cmd::SetSandbox(value) => {
                emit(&ev_tx, Event::Log(format!("sandbox set: {value}")));
                state.sandbox = Some(value);
            }
            Cmd::SetReasoning(value) => {
                emit(&ev_tx, Event::Log(format!("reasoning set: {value}")));
                state.pick = Some(value);
                let wanted = state.pick.clone();
                state.reasoning = state.reconcile(wanted.as_deref());
                state.emit(cfg.agent, &ev_tx);
            }
            Cmd::SetMode(value) => {
                emit(
                    &ev_tx,
                    Event::Log(format!("no mode config option to set: {value}")),
                );
            }
        }
    }

    // Same as the ACP backend: wake anything still waiting on a decision, so
    // the agent is never left hanging on a connection that is going away.
    if let Some(table) = shared.permissions.upgrade() {
        table.lock().unwrap().clear();
    }
    emit(&ev_tx, Event::Log("connection closing".into()));
    Ok(())
}

/// Send a request and deserialize its response, naming the method in any error
/// so a schema mismatch is legible in the log.
async fn request<T: serde::de::DeserializeOwned>(
    transport: &Transport,
    method: &str,
    params: Value,
) -> Result<T, String> {
    let value = transport.request(method, params).await?;
    serde_json::from_value(value).map_err(|err| format!("{method}: unexpected response: {err}"))
}

/// Run one turn and wait for it to end.
///
/// The turn's output — messages, thinking, tool rows, approvals — streams in on
/// the handler task meanwhile; starting the turn and reporting its end is all
/// this does, and is what keeps the command loop serial.
async fn run_turn(
    transport: &Transport,
    shared: &Shared,
    ev_tx: &Sender<Event>,
    thread_id: &str,
    overrides: TurnOverrides<'_>,
    text: &str,
) {
    // Installed before the turn starts, so a completion cannot arrive before
    // anyone is listening for it.
    let (done_tx, done_rx) = oneshot::channel();
    *shared.turn_done.lock().unwrap() = Some(done_tx);
    let mut params = json!({
        "threadId": thread_id,
        "input": [{ "type": "text", "text": text }],
    });
    if let Some(model) = overrides.model {
        params["model"] = json!(model);
    }
    // Repeated on every turn, not just on the thread, so a pick made
    // mid-session takes effect on the next turn without reopening anything —
    // the protocol makes a turn-level override the thread's default from then
    // on. An unrecognised value yields nothing, leaving the current policy be.
    if let Some(policy) = overrides.sandbox.and_then(convert::sandbox_policy) {
        params["sandboxPolicy"] = policy;
    }
    // Per turn, and only per turn: `thread/start` takes no effort, so this is
    // the one place a reasoning pick can take effect. The value is an open
    // string advertised by the model, so it is passed through rather than
    // validated — an unknown one is the agent's to reject, not kumu's to guess.
    if let Some(effort) = overrides.reasoning {
        params["effort"] = json!(effort);
    }
    emit(ev_tx, Event::Log("prompt sent".into()));
    match request::<TurnStartResult>(transport, "turn/start", params).await {
        Ok(started) => emit(
            ev_tx,
            Event::Log(format!("turn started: {}", started.turn.id)),
        ),
        Err(err) => {
            // Nothing will complete, so drop the waiter rather than leave the
            // next prompt to inherit it.
            shared.turn_done.lock().unwrap().take();
            emit(ev_tx, Event::Error(err));
            return;
        }
    }
    match done_rx.await {
        Ok(turn) => {
            // Flushed first, then the failure. The UI drops a session's
            // in-progress turn when it is told about an error, so reporting
            // before `Done` would throw away the work the turn had shown.
            emit(ev_tx, Event::Done(turn.status.clone()));
            if turn.status == "failed" {
                let message = turn
                    .error
                    .map(|error| error.message)
                    .unwrap_or_else(|| "本轮失败".to_string());
                emit(ev_tx, Event::Error(message));
            }
        }
        // The handler went away with the connection.
        Err(_) => emit(ev_tx, Event::Done("closed".into())),
    }
}

/// Fold server messages into events until the connection ends.
async fn handle_messages(shared: Arc<Shared>, mut messages: mpsc::UnboundedReceiver<Message>) {
    while let Some(message) = messages.recv().await {
        match message {
            Message::Notification { method, params } => {
                handle_notification(&shared, &method, params)
            }
            Message::Request { id, method, params } => handle_request(&shared, id, &method, params),
        }
    }
}

/// Turn one notification into events.
fn handle_notification(shared: &Shared, method: &str, params: Value) {
    match method {
        "item/agentMessage/delta" => {
            if let Ok(delta) = serde_json::from_value::<DeltaNotification>(params) {
                emit(&shared.ev_tx, Event::Agent(delta.delta));
            }
        }
        // Both the readable summary and raw reasoning become thinking text;
        // which one the model supports is not kumu's business.
        "item/reasoning/summaryTextDelta" | "item/reasoning/textDelta" => {
            if let Ok(delta) = serde_json::from_value::<DeltaNotification>(params) {
                emit(&shared.ev_tx, Event::Thought(delta.delta));
            }
        }
        "item/commandExecution/outputDelta" => {
            let Ok(delta) = serde_json::from_value::<DeltaNotification>(params) else {
                return;
            };
            // The row's content is replaced, not appended to, so the whole
            // accumulated output is what has to be sent.
            let content = {
                let mut output = shared.output.lock().unwrap();
                let accumulated = output.entry(delta.item_id.clone()).or_default();
                accumulated.push_str(&delta.delta);
                accumulated.clone()
            };
            emit(
                &shared.ev_tx,
                Event::ToolCall {
                    id: delta.item_id,
                    title: String::new(),
                    status: "",
                    content,
                },
            );
        }
        "item/started" | "item/completed" => {
            let Ok(notification) = serde_json::from_value::<ItemNotification>(params) else {
                return;
            };
            let item = notification.item;
            let detail = convert::item_content(&item);
            if !detail.is_empty() {
                shared
                    .details
                    .lock()
                    .unwrap()
                    .insert(item.id().to_string(), detail);
            }
            if method == "item/completed" {
                // The completed item carries its own aggregated output, so the
                // deltas are dropped rather than left to outlive it.
                shared.output.lock().unwrap().remove(item.id());
            }
            if let Some(event) = convert::tool_event(&item) {
                emit(&shared.ev_tx, event);
            }
        }
        "turn/completed" => {
            let Ok(notification) = serde_json::from_value::<TurnCompletedNotification>(params)
            else {
                return;
            };
            shared.output.lock().unwrap().clear();
            if let Some(done) = shared.turn_done.lock().unwrap().take() {
                let _ = done.send(notification.turn);
            }
        }
        "error" => {
            let Ok(notification) = serde_json::from_value::<ErrorNotification>(params) else {
                return;
            };
            // Codex retries transient failures itself, and a final one is
            // reported from the turn's own completion, so this is context for
            // the log rather than something to show twice.
            emit(
                &shared.ev_tx,
                Event::Log(format!(
                    "error{}: {}",
                    if notification.will_retry {
                        " (retrying)"
                    } else {
                        ""
                    },
                    notification.error.message
                )),
            );
        }
        // Lifecycle chatter that carries nothing the UI shows. It is listed
        // rather than pattern-matched so that a method this build has never
        // seen still falls through to the log below and announces itself —
        // which is the canary for app-server growing a new notification.
        "turn/started"
        | "thread/started"
        | "thread/status/changed"
        | "serverRequest/resolved"
        | "mcpServer/startupStatus/updated"
        | "remoteControl/status/changed"
        | "hook/started"
        | "hook/completed"
        | "thread/tokenUsage/updated"
        | "account/rateLimits/updated" => {}
        other => emit(&shared.ev_tx, Event::Log(format!("update: {other}"))),
    }
}

/// Answer a server-initiated request.
///
/// Approvals become prompts attached to the row they belong to. Anything else
/// is refused: an unanswered request parks the turn forever, so silence is the
/// one response that is never safe.
fn handle_request(shared: &Arc<Shared>, id: Value, method: &str, params: Value) {
    let prompt = match method {
        "item/commandExecution/requestApproval" => {
            serde_json::from_value::<CommandApprovalParams>(params)
                .ok()
                .map(|params| ApprovalPrompt {
                    title: params
                        .command
                        .clone()
                        .unwrap_or_else(|| "命令执行".to_string()),
                    detail: command_detail(&params, shared),
                    item_id: params.item_id,
                })
        }
        "item/fileChange/requestApproval" => {
            serde_json::from_value::<FileChangeApprovalParams>(params)
                .ok()
                .map(|params| ApprovalPrompt {
                    title: "文件改动".to_string(),
                    detail: file_change_detail(&params, shared),
                    item_id: params.item_id,
                })
        }
        _ => None,
    };

    let Some(prompt) = prompt else {
        emit(
            &shared.ev_tx,
            Event::Log(format!("unsupported request: {method}")),
        );
        shared
            .transport
            .respond_error(id, &format!("kumu does not support {method}"));
        return;
    };

    // No UI left to ask, so the turn has to be abandoned. `cancel` is the only
    // answer that cannot be mistaken for consent.
    let Some(table) = shared.permissions.upgrade() else {
        shared.transport.respond(id, decision_payload(None));
        return;
    };

    let request_id = format!(
        "perm-{}",
        shared.next_request_id.fetch_add(1, Ordering::Relaxed)
    );
    let (tx, rx) = oneshot::channel();
    table.lock().unwrap().insert(request_id.clone(), tx);
    // Released before spawning below, so the guard never crosses an await.
    drop(table);
    emit(
        &shared.ev_tx,
        Event::PermissionRequest {
            tool_call_id: prompt.item_id,
            request_id,
            title: prompt.title,
            detail: prompt.detail,
            options: convert::approval_options(),
        },
    );

    // Waiting here would stall every notification queued behind it, so the
    // answer is awaited on its own task; the wire id travels with it because
    // the transport needs the agent's id, not ours.
    let transport = shared.transport.clone();
    let ev_tx = shared.ev_tx.clone();
    tokio::spawn(async move {
        let option_id = rx.await.ok().flatten();
        emit(
            &ev_tx,
            Event::Log(format!("permission answered: {option_id:?}")),
        );
        transport.respond(id, decision_payload(option_id.as_deref()));
    });
}

/// The response body for an approval. The protocol wraps the decision in an
/// object; it is not a bare string.
fn decision_payload(option_id: Option<&str>) -> Value {
    json!({ "decision": convert::decision(option_id) })
}

/// What a command approval shows: why, and where it would run. The command
/// itself is already the prompt's title.
fn command_detail(params: &CommandApprovalParams, shared: &Shared) -> String {
    let mut detail = String::new();
    if let Some(reason) = &params.reason {
        detail.push_str(reason);
    }
    if let Some(cwd) = &params.cwd {
        push_line(&mut detail, &format!("工作目录：{cwd}"));
    }
    if detail.is_empty() {
        detail = item_detail(shared, &params.item_id);
    }
    detail
}

/// What a file-change approval shows: why, and the diff the item proposed.
fn file_change_detail(params: &FileChangeApprovalParams, shared: &Shared) -> String {
    let mut detail = String::new();
    if let Some(reason) = &params.reason {
        detail.push_str(reason);
    }
    if let Some(root) = &params.grant_root {
        push_line(&mut detail, &format!("请求写入权限：{root}"));
    }
    if detail.is_empty() {
        detail = item_detail(shared, &params.item_id);
    }
    detail
}

/// The content remembered for an item when it started, so a request carrying
/// only an item id can still show what it is about.
fn item_detail(shared: &Shared, item_id: &str) -> String {
    shared
        .details
        .lock()
        .unwrap()
        .get(item_id)
        .cloned()
        .unwrap_or_default()
}

/// Append a line, keeping exactly one newline between parts.
fn push_line(buffer: &mut String, line: &str) {
    if !buffer.is_empty() && !buffer.ends_with('\n') {
        buffer.push('\n');
    }
    buffer.push_str(line);
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::backend::test_support::{AGENT_TEST_LOCK, command_exists};
    use crate::backend::{Cmd, Config, Event, PermissionKind, spawn};

    /// End-to-end: spawn a real Codex session, send one prompt, and expect
    /// streamed agent text followed by `Done` — the same path the home window
    /// takes. Skipped when the binary is absent; a Codex that is not signed in
    /// makes the session say so, and being signed in is a property of the
    /// machine rather than of this code, so that is skipped too.
    ///
    /// This is also the proof that a sandbox pick reaches Codex: the session is
    /// told to use `read-only` and then has to report that back, which only the
    /// agent can confirm. A reasoning pick is sent with it and rides on the
    /// turn, so a turn that completes is the agent accepting the effort.
    #[test]
    fn session_prompts_codex_end_to_end() {
        let _guard = AGENT_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut cfg = Config::for_agent("codex").expect("codex is a registered agent");
        if !command_exists(&cfg.command) {
            println!(
                "skipping session_prompts_codex_end_to_end: {} not found",
                cfg.command
            );
            return;
        }
        cfg.auto_prompt = None;
        let (tx, rx) = std::sync::mpsc::channel();
        let conn = spawn(
            cfg,
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
            tx,
        );

        // Sent immediately after spawning, exactly as the UI does it, so the
        // session can configure the thread itself instead of only its turns.
        // This machine's Codex configuration says `danger-full-access`, so a
        // `read-only` coming back is a difference only kumu could have caused.
        conn.send(Cmd::SetSandbox("read-only".into()));

        // A turn can involve a round trip to the service, so this is generous.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);

        // The catalogues arrive before any prompt can be sent. Selecting the
        // agent's own default also exercises the per-turn model override.
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match rx.recv_timeout(remaining).expect("catalog timed out") {
                Event::SessionOptions {
                    models,
                    current_model,
                    sandboxes,
                    current_sandbox,
                    reasoning,
                    ..
                } => {
                    assert!(
                        sandboxes.iter().any(|s| s.value == "read-only"),
                        "the sandbox picker should offer read-only: {sandboxes:?}"
                    );
                    assert_eq!(
                        current_sandbox.as_deref(),
                        Some("read-only"),
                        "codex should report the policy kumu asked for, not its configured one"
                    );
                    // The reasoning list is the session-level reduction of the
                    // selected model's set, so a pick off it is one the model
                    // advertises.
                    assert!(
                        !reasoning.is_empty(),
                        "the default model should offer reasoning efforts"
                    );
                    let pick = current_model
                        .clone()
                        .or_else(|| models.first().map(|model| model.value.clone()));
                    let effort = reasoning.first().map(|effort| effort.value.clone());
                    if let Some(value) = pick {
                        conn.send(Cmd::SetModel(value));
                    }
                    if let Some(effort) = effort {
                        conn.send(Cmd::SetReasoning(effort));
                    }
                    break;
                }
                Event::Error(err) => {
                    println!("skipping session_prompts_codex_end_to_end: {err}");
                    return;
                }
                _ => {}
            }
        }
        conn.send(Cmd::Prompt("Reply with exactly: ok".into()));

        let mut text = String::new();
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match rx.recv_timeout(remaining).expect("prompt timed out") {
                Event::Agent(chunk) => text.push_str(&chunk),
                // Nobody is there to click, so take the first allow option and
                // let the turn finish; this also exercises the answer path.
                Event::PermissionRequest {
                    request_id,
                    options,
                    ..
                } => {
                    let choice = options
                        .iter()
                        .find(|option| {
                            matches!(
                                option.kind,
                                PermissionKind::AllowOnce | PermissionKind::AllowAlways
                            )
                        })
                        .or_else(|| options.first())
                        .map(|option| option.id.clone());
                    conn.resolve_permission(&request_id, choice);
                }
                Event::Done(_) => {
                    println!("codex replied: {text:?}");
                    assert!(
                        text.to_lowercase().contains("ok"),
                        "unexpected reply: {text:?}"
                    );
                    break;
                }
                Event::Error(err) => panic!("codex session error: {err}"),
                _ => {}
            }
        }
    }
}
