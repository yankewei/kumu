//! The Claude Code session loop: one `claude -p` process per session, driven
//! over newline-delimited JSON.
//!
//! Same contract as the other backends: it produces [`Event`]s, consumes
//! [`Cmd`]s, and answers approvals through the shared permission table, so
//! nothing above `backend` knows which agent is talking.
//!
//! Three things about this protocol shape the code below:
//!
//!   - **The host is asked, not told.** The CLI only raises permission requests
//!     when it was launched with `--permission-prompt-tool stdio` (see
//!     `config::Config::claude`); each one is a `control_request` that must be
//!     answered, or the turn parks behind a prompt nobody sees.
//!   - **Text arrives twice.** With `--include-partial-messages` the reply
//!     streams as deltas and then repeats inside the complete `assistant`
//!     message, so the message ids that streamed are remembered and their
//!     `assistant` twin is skipped. The CLI's own replies (to `/model`, say) do
//!     not stream, and are shown exactly once, by the other half of that rule.
//!   - **A result is not always a turn.** Slash commands sent to change an axis
//!     answer with a `result` too, so what was sent is queued and each result
//!     settles the oldest entry: only a prompt's result ends the turn.

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use super::convert;
use super::protocol::{self, ContentBlock, Incoming, ResultEvent};
use super::settings;
use super::transport::{self, Message, Transport, debug_log};
use crate::backend::config::{Config, SessionPicks};
use crate::backend::connection::WeakPermissions;
use crate::backend::event::{
    AccessInfo, Cmd, Event, ModeInfo, ModelInfo, ReasoningInfo, SandboxInfo, emit,
};
use crate::backend::session::{
    CANCELLED, CancelRequests, PermissionQuestion, SessionOptionsBuilder, ask_user,
    forget_stale_cancels, release_pending_permissions, settle,
};

/// What the next `result` line settles.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Awaiting {
    /// A user prompt: its result is where the turn ends.
    Prompt,
    /// A command sent as a message (`/model`, `/effort`): its result must not
    /// end the turn, because no turn was opened for it.
    Command,
}

/// The catalogue, and what is in force on each axis.
struct Options {
    models: Vec<ModelInfo>,
    current_model: Option<String>,
    modes: Vec<ModeInfo>,
    current_mode: Option<String>,
    access: Vec<AccessInfo>,
    current_access: Option<String>,
    sandboxes: Vec<SandboxInfo>,
    current_sandbox: Option<String>,
    reasoning: Vec<ReasoningInfo>,
    current_reasoning: Option<String>,
}

/// What the message pump and the command loop share.
struct Shared {
    /// The registry id, stamped on every event this session emits.
    agent: &'static str,
    transport: Arc<Transport>,
    ev_tx: Sender<Event>,
    permissions: WeakPermissions,
    /// Distinguishes permission requests on this connection; the CLI does not
    /// give kumu one of its own.
    next_request_id: AtomicU64,
    /// What each `result` line settles, oldest first.
    awaiting: Mutex<VecDeque<Awaiting>>,
    /// Set while a prompt is in flight, so the pump can wake the command loop.
    turn_done: Mutex<Option<oneshot::Sender<ResultEvent>>>,
    /// Message ids whose text and thinking already arrived as deltas.
    streamed: Mutex<HashSet<String>>,
    options: Mutex<Options>,
    /// Whether an interrupt was asked for the turn now in flight, so the pump
    /// reports its `result` as a cancel rather than as a failure. Cleared when
    /// the next prompt starts, since only one turn is ever open.
    cancelled: AtomicBool,
    /// What this connection was launched with, so a command that only repeats
    /// it does not need saying again.
    launched: SessionPicks,
}

impl Shared {
    /// Report the catalogue and what is in force. Sent at the handshake, and
    /// again whenever an axis changes, matching the other backends.
    fn emit_options(&self) {
        let options = self.options.lock().unwrap();
        let event = SessionOptionsBuilder::new(self.agent)
            .models(options.models.clone(), options.current_model.clone())
            .modes(options.modes.clone(), options.current_mode.clone())
            .access(options.access.clone(), options.current_access.clone())
            .sandboxes(options.sandboxes.clone(), options.current_sandbox.clone())
            .reasoning(options.reasoning.clone(), options.current_reasoning.clone())
            .build();
        emit(&self.ev_tx, event);
    }

    /// Send a slash command as a user message. Its result settles the queue
    /// without ending a turn, so it is queued as a command.
    fn send_command(&self, text: &str) {
        self.awaiting.lock().unwrap().push_back(Awaiting::Command);
        self.transport.send(user_message(text));
    }
}

/// The effort in force: kumu's own flag when it launched with one, else what
/// Claude Code's configuration saves for this model.
///
/// A launch flag is an explicit choice and outranks a saved level, which is the
/// order the CLI resolves them in. `model` is the model the session reported,
/// when it has reported one: a saved level is keyed by model, so the answer gets
/// closer once `system/init` names it.
fn effort_in_force(launched: &SessionPicks, model: Option<&str>) -> Option<String> {
    launched
        .reasoning
        .clone()
        .or_else(|| settings::effort_in_force(model))
}

/// What the reasoning chip shows: the level in force, else the model's own
/// documented default.
///
/// The CLI reports the level it resolved to nowhere, so a session that is
/// neither launched at one nor covered by a saved one leaves the axis with only
/// the model table to answer from — and it does answer, since every model that
/// supports effort runs at one. [`convert::default_effort`] holds the table.
fn reported_effort(launched: &SessionPicks, model: Option<&str>) -> String {
    effort_in_force(launched, model).unwrap_or_else(|| convert::default_effort(model).to_string())
}

pub(in crate::backend) async fn run(
    cfg: Config,
    cwd: PathBuf,
    ev_tx: Sender<Event>,
    mut cmd_rx: mpsc::UnboundedReceiver<Cmd>,
    mut cancels: CancelRequests,
    permissions: WeakPermissions,
) -> Result<(), String> {
    let (transport, mut messages, _child) =
        transport::spawn(&cfg.command, &cfg.args, &cfg.env, &cwd)
            .map_err(|error| format!("failed to launch {}: {error}", cfg.command))?;
    let transport = Arc::new(transport);

    // The handshake answers with the account's catalogue and the mode in force,
    // and must precede the first user message. `system/init` — which repeats the
    // mode and names the resolved model — only arrives with the first turn, so
    // this is the only place the pickers can be filled before a prompt.
    let reported: protocol::InitializeResponse = transport
        .control(json!({"subtype": "initialize", "hooks": null}))
        .await
        .map(|payload| serde_json::from_value(payload).unwrap_or_default())?;

    let shared = Arc::new(Shared {
        agent: cfg.agent,
        transport: transport.clone(),
        ev_tx: ev_tx.clone(),
        permissions,
        next_request_id: AtomicU64::new(1),
        awaiting: Mutex::new(VecDeque::new()),
        turn_done: Mutex::new(None),
        streamed: Mutex::new(HashSet::new()),
        options: Mutex::new(Options {
            models: convert::session_models(&reported.models),
            // Until a session says otherwise, nothing is claimed: the mode
            // sentinel and the model report both arrive with `system/init`.
            current_model: None,
            modes: convert::mode_options(),
            current_mode: convert::current_mode(reported.current_permission_mode.as_deref()),
            access: convert::access_options(),
            current_access: convert::current_access(reported.current_permission_mode.as_deref()),
            // Claude's sandbox is an OS-level boundary in its own settings with
            // no per-session form, so this backend offers no sandbox axis: the
            // access policy above is what carries "how much may it do on its own".
            sandboxes: Vec::new(),
            current_sandbox: None,
            reasoning: convert::session_reasoning(&reported.models),
            // The CLI reports no effort in force on either the handshake or
            // `system/init`, so this is read from its own configuration — or,
            // failing that, the model's documented default. The session's own
            // model pick is the best name available until `system/init` says
            // which model the pick resolved to.
            current_reasoning: Some(reported_effort(&cfg.session, cfg.session.model.as_deref())),
        }),
        launched: cfg.session.clone(),
        cancelled: AtomicBool::new(false),
    });
    shared.emit_options();

    // The pump owns the CLI's side: stream lines, tool rows, and the permission
    // prompts that arrive mid-turn.
    let pump = {
        let shared = shared.clone();
        tokio::spawn(async move {
            while let Some(message) = messages.recv().await {
                handle_message(&shared, message);
            }
        })
    };

    // Headless convenience, matching the ACP and Codex backends: one prompt
    // before the UI loop starts.
    if let Some(auto) = cfg.auto_prompt.clone() {
        emit(&ev_tx, Event::Log(format!("auto prompt: {auto}")));
        prompt(&shared, &auto, &mut cancels).await;
    }

    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            Cmd::Prompt(text) => prompt(&shared, &text, &mut cancels).await,
            Cmd::SetModel(value) => {
                // The model is a launch flag, so a change mid-session rides the
                // documented `/model` command instead.
                if shared.launched.model.as_deref() != Some(value.as_str()) {
                    shared.send_command(&format!("/model {value}"));
                    shared.options.lock().unwrap().current_model = Some(value);
                    shared.emit_options();
                }
            }
            Cmd::SetReasoning(value) => {
                if shared.launched.reasoning.as_deref() != Some(value.as_str()) {
                    shared.send_command(&format!("/effort {value}"));
                    shared.options.lock().unwrap().current_reasoning = Some(value);
                    shared.emit_options();
                }
            }
            Cmd::SetMode(value) => {
                if shared.launched.mode.as_deref() != Some(value.as_str()) {
                    let request = json!({"subtype": "set_permission_mode", "mode": value});
                    match shared.transport.control(request).await {
                        Ok(_) => {
                            {
                                // Both chips are projections of this one value,
                                // so a pick on either moves both.
                                let mut options = shared.options.lock().unwrap();
                                options.current_mode = convert::current_mode(Some(&value));
                                options.current_access = convert::current_access(Some(&value));
                            }
                            shared.emit_options();
                        }
                        Err(error) => emit(
                            &shared.ev_tx,
                            Event::Log(format!("切换权限模式失败：{error}")),
                        ),
                    }
                }
            }
            Cmd::SetSandbox(value) => {
                // This backend offers no sandbox axis — Claude's sandbox lives
                // in its own settings with no per-session form — so the UI has
                // no chip for one and this should not arrive. Saying so beats
                // dropping it silently, the same answer the ACP backend gives.
                emit(
                    &shared.ev_tx,
                    Event::Log(format!("no sandbox to set: {value}")),
                );
            }
        }
    }

    // The command channel closed, so the UI is gone. Release anyone still
    // waiting on a decision, then let the child go.
    release_pending_permissions(&shared.permissions);
    pump.abort();
    emit(&ev_tx, Event::Log("connection closing".to_string()));
    Ok(())
}

/// Send one prompt and wait for the turn it opens to end.
async fn prompt(shared: &Arc<Shared>, text: &str, cancels: &mut CancelRequests) {
    forget_stale_cancels(cancels);
    shared.cancelled.store(false, Ordering::Relaxed);
    let (tx, rx) = oneshot::channel();
    *shared.turn_done.lock().unwrap() = Some(tx);
    shared.awaiting.lock().unwrap().push_back(Awaiting::Prompt);
    shared.transport.send(user_message(text));
    // The interrupt has to reach the CLI while the turn is running, so it is
    // watched for here rather than on the command channel: the loop is parked on
    // this very await.
    tokio::pin!(rx);
    let mut asked = false;
    loop {
        tokio::select! {
            _ = &mut rx => break,
            _ = cancels.recv(), if !asked => {
                asked = true;
                shared.cancelled.store(true, Ordering::Relaxed);
                interrupt(shared).await;
            }
        }
    }
}

/// Ask the CLI to interrupt the turn in flight.
///
/// The CLI answers this control request with `success`, and then ends the turn
/// with an ordinary `result` line — which is what the pump settles on, and the
/// same thing waku's Claude driver relies on. What that `result` says about the
/// interruption is not documented and was not captured, so the turn is marked
/// cancelled on this side and the settle pass reads the marking rather than the
/// CLI's wording (see `Shared::cancelled`).
async fn interrupt(shared: &Arc<Shared>) {
    emit(&shared.ev_tx, Event::Log("cancel requested".into()));
    if let Err(error) = shared
        .transport
        .control(json!({"subtype": "interrupt"}))
        .await
    {
        emit(
            &shared.ev_tx,
            Event::Log(format!("interrupt 失败：{error}")),
        );
    }
}

/// The CLI's shape for a user message on stdin.
fn user_message(text: &str) -> Value {
    json!({
        "type": "user",
        "message": {"role": "user", "content": text},
    })
}

fn handle_message(shared: &Arc<Shared>, message: Message) {
    match message {
        Message::Request {
            request_id,
            request,
        } => on_control_request(shared, &request_id, request),
        Message::Line(line) => {
            let Ok(incoming) = serde_json::from_value::<Incoming>(line) else {
                // The transport already named the line under KUMU_DEBUG; a shape
                // kumu cannot read is not worth a row in the conversation.
                return;
            };
            match incoming {
                Incoming::System(system) => on_system(shared, system),
                Incoming::Assistant(message) => on_assistant(shared, &message),
                Incoming::User(message) => on_user(shared, &message),
                Incoming::StreamEvent(event) => on_stream_event(shared, &event),
                Incoming::Result(result) => on_result(shared, result),
                // A control line never reaches here: the transport consumes a
                // `control_response` and hands a `control_request` over as a
                // `Message::Request`. Anything left is a kind this build does
                // not model.
                Incoming::Other => {}
            }
        }
    }
}

/// `system` messages are the session's own report plus its progress chatter.
fn on_system(shared: &Shared, system: protocol::SystemEvent) {
    match system.subtype.as_str() {
        // The session's own report, arriving with the first turn: the mode in
        // force and the model kumu's pick resolved to.
        "init" => {
            let model = system.model.filter(|model| !model.is_empty());
            {
                let mut options = shared.options.lock().unwrap();
                if let Some(model) = model.clone() {
                    options.current_model = Some(model);
                }
                if system.permission_mode.is_some() {
                    // One reported value, two projections: plan is the work
                    // mode, anything else is the access policy in force.
                    options.current_mode = convert::current_mode(system.permission_mode.as_deref());
                    options.current_access =
                        convert::current_access(system.permission_mode.as_deref());
                }
                // The model is what a saved effort level is keyed by, so this
                // report is the first chance to look up the right one — and the
                // only chance to apply the right model's default.
                options.current_reasoning =
                    Some(reported_effort(&shared.launched, model.as_deref()));
            }
            shared.emit_options();
        }
        // One event per thinking chunk — 674 of them in one short turn — so
        // this is a subtype to drop rather than to show.
        "thinking_tokens" | "status" => {}
        // Worth a line: a retrying turn would otherwise look stuck. The counters
        // are the documented shape of this event, not a captured one.
        "api_retry" => {
            let attempt = match (system.attempt, system.max_retries) {
                (Some(attempt), Some(max)) => format!("（第 {attempt}/{max} 次）"),
                (Some(attempt), None) => format!("（第 {attempt} 次）"),
                _ => String::new(),
            };
            emit(&shared.ev_tx, Event::Log(format!("接口重试{attempt}")));
        }
        // The refused call already shows up as a failed tool row carrying the
        // CLI's own explanation; this only names it under KUMU_DEBUG.
        "permission_denied" => debug_log(&format!(
            "permission_denied: {} ({})",
            system.tool_name.unwrap_or_default(),
            system.message.unwrap_or_default()
        )),
        other => debug_log(other),
    }
}

/// An `assistant` message: the complete blocks of a reply, after the same
/// blocks' deltas have already been streamed.
fn on_assistant(shared: &Shared, message: &protocol::AssistantMessage) {
    let id = message.message.id.as_str();
    let streamed = !id.is_empty() && shared.streamed.lock().unwrap().contains(id);
    // A message whose content is a bare string is text without blocks; the CLI
    // sends that shape for some of its own replies.
    if let protocol::ContentBlocks::Text(text) = &message.message.content {
        if !streamed && !text.is_empty() {
            emit(&shared.ev_tx, Event::Agent(text.clone()));
        }
        return;
    }
    for block in message.message.content.blocks() {
        match block {
            ContentBlock::ToolUse { id, name, input } => emit(
                &shared.ev_tx,
                Event::ToolCall {
                    id: id.clone(),
                    title: convert::tool_title(name, input),
                    status: "running",
                    // The call's output arrives with its result; an empty body
                    // is what keeps the row from opening one.
                    content: String::new(),
                },
            ),
            // Text and thinking already arrived as deltas, so only a message
            // that did not stream — the CLI's own replies — is shown here.
            ContentBlock::Text { text } if !streamed => {
                emit(&shared.ev_tx, Event::Agent(text.clone()))
            }
            ContentBlock::Thinking { thinking } if !streamed => {
                emit(&shared.ev_tx, Event::Thought(thinking.clone()))
            }
            _ => {}
        }
    }
}

/// A `user` message from the CLI is a tool result coming back.
fn on_user(shared: &Shared, message: &protocol::AssistantMessage) {
    for block in message.message.content.blocks() {
        let ContentBlock::ToolResult {
            tool_use_id,
            is_error,
            content,
        } = block
        else {
            continue;
        };
        emit(
            &shared.ev_tx,
            Event::ToolCall {
                id: tool_use_id.clone(),
                // Empty fields leave the row's title alone.
                title: String::new(),
                status: if *is_error { "failed" } else { "done" },
                content: convert::tool_content(content),
            },
        );
    }
}

/// A raw API streaming event: the only place text and thinking arrive
/// incrementally.
fn on_stream_event(shared: &Shared, event: &protocol::StreamEvent) {
    let event = &event.event;
    if let Some(id) = event.message_id() {
        // Remember that this message streamed, so its `assistant` twin is not
        // appended a second time.
        shared.streamed.lock().unwrap().insert(id.to_string());
    }
    if event.kind != "content_block_delta" {
        return;
    }
    match event.delta.kind.as_str() {
        "text_delta" => {
            if let Some(text) = &event.delta.text {
                emit(&shared.ev_tx, Event::Agent(text.clone()));
            }
        }
        "thinking_delta" => {
            if let Some(thinking) = &event.delta.thinking {
                emit(&shared.ev_tx, Event::Thought(thinking.clone()));
            }
        }
        // Tool input and signatures are not rendered: the complete block comes
        // with the `assistant` message.
        _ => {}
    }
}

/// The last line of a turn, and of every command sent as a message.
fn on_result(shared: &Shared, result: ResultEvent) {
    match shared.awaiting.lock().unwrap().pop_front() {
        Some(Awaiting::Command) => {
            if result.is_error {
                emit(
                    &shared.ev_tx,
                    Event::Log(format!("指令未生效：{}", result.failure())),
                );
            }
        }
        // A prompt's result, or one nobody claimed — a turn the CLI ended on
        // its own. Either way the UI's turn is over.
        Some(Awaiting::Prompt) | None => {
            // An interrupted turn is reported as cancelled rather than as
            // whatever the CLI calls it, and its failure — if it reports one for
            // being interrupted at all — is not shown as a failure, because the
            // user asked for it.
            let cancelled = shared.cancelled.load(Ordering::Relaxed);
            let stop = if cancelled {
                CANCELLED.to_string()
            } else {
                result
                    .stop_reason
                    .clone()
                    .unwrap_or_else(|| result.subtype.clone())
            };
            let failure = (!cancelled && result.is_error).then(|| result.failure());
            for event in settle(stop, failure) {
                emit(&shared.ev_tx, event);
            }
            if !result.permission_denials.is_empty() {
                emit(
                    &shared.ev_tx,
                    Event::Log(format!(
                        "{} 次调用未获授权",
                        result.permission_denials.len()
                    )),
                );
            }
            if let Some(tx) = shared.turn_done.lock().unwrap().take() {
                let _ = tx.send(result);
            }
        }
    }
}

/// A request from the CLI. `can_use_tool` is the one kumu answers; the others
/// belong to hosts that integrate more deeply.
fn on_control_request(shared: &Arc<Shared>, request_id: &str, body: protocol::ControlRequestBody) {
    match body.subtype.as_str() {
        "can_use_tool" => on_can_use_tool(shared, request_id, body),
        // Answering with an error lets the turn fail cleanly; silence would park
        // it on a prompt nobody can see.
        other => {
            debug_log(&format!("unsupported control request: {other}"));
            shared
                .transport
                .respond_error(request_id, &format!("kumu 未实现的控制请求：{other}"));
        }
    }
}

/// The permission round trip, the same as the other backends: hand the question
/// to the UI through the shared table, and answer the CLI when it answers.
fn on_can_use_tool(shared: &Arc<Shared>, request_id: &str, body: protocol::ControlRequestBody) {
    let Some(table) = shared.permissions.upgrade() else {
        // No UI is left to ask, so the honest answer is a refusal: the CLI would
        // otherwise park a turn nobody can unblock.
        shared
            .transport
            .respond(request_id, deny_response("会话已关闭"));
        return;
    };
    let transport = shared.transport.clone();
    let input = body.input.clone();
    let agent_request_id = request_id.to_string();
    ask_user(
        table,
        &shared.next_request_id,
        &shared.ev_tx,
        PermissionQuestion {
            tool_call_id: body.tool_use_id.clone().unwrap_or_default(),
            title: convert::permission_title(&body),
            detail: convert::permission_detail(&body),
            options: convert::permission_options(),
        },
        move |option_id| {
            let allowed = option_id.as_deref() == Some(convert::ALLOW_OPTION);
            let response = if allowed {
                // The input is echoed back unchanged, which is what the CLI
                // requires of an allow answer.
                json!({"behavior": "allow", "updatedInput": input})
            } else {
                deny_response("用户拒绝了这次调用")
            };
            transport.respond(&agent_request_id, response);
        },
    );
}

/// The refusal the CLI accepts, whether the user said no or nobody could ask.
fn deny_response(message: &str) -> Value {
    json!({"behavior": "deny", "message": message})
}

#[cfg(test)]
mod tests {
    use crate::backend::test_support::{AGENT_TEST_LOCK, command_exists};
    use crate::backend::{Cmd, Config, Event, PermissionKind, SessionPicks, spawn};

    /// End-to-end: spawn a real `claude -p`, take one turn, and walk the whole
    /// path the home window walks — the handshake catalogue, streamed text, a
    /// tool call whose permission the host has to grant, and `Done`.
    ///
    /// Two properties of the machine are treated as skip conditions rather than
    /// failures, matching the ACP and Codex end-to-end tests: the `claude`
    /// binary may be absent, and a CLI with no credentials says so instead of
    /// answering.
    ///
    /// The run is given a scratch `HOME` and `CLAUDE_CONFIG_DIR`, so its
    /// transcript lands there and none of the user's own Claude Code settings or
    /// history are involved. Authentication comes from the environment
    /// (`ANTHROPIC_AUTH_TOKEN` here) or the system keychain, neither of which the
    /// scratch home hides — checked by running this against an empty home.
    #[test]
    fn session_prompts_claude_end_to_end() {
        let _guard = AGENT_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut cfg = Config::for_agent("claude").expect("claude is a registered agent");
        if !command_exists(&cfg.command) {
            println!(
                "skipping session_prompts_claude_end_to_end: {} not found",
                cfg.command
            );
            return;
        }
        cfg.auto_prompt = None;
        let home = std::env::temp_dir().join(format!("kumu-claude-test-{}", std::process::id()));
        std::fs::create_dir_all(&home).expect("a scratch home");
        cfg.env
            .push(("HOME".to_string(), home.to_string_lossy().into_owned()));
        cfg.env.push((
            "CLAUDE_CONFIG_DIR".to_string(),
            home.join(".claude").to_string_lossy().into_owned(),
        ));
        // The session's work mode rides the command line, the way the UI opens a
        // connection: `--permission-mode` is where Claude takes it, and plan is
        // how the one value expresses that axis.
        let cfg = cfg.with_session(SessionPicks {
            mode: Some("plan".to_string()),
            ..SessionPicks::default()
        });

        let (tx, rx) = std::sync::mpsc::channel();
        // The turn runs in the scratch directory, so a file it writes lands
        // there and not in whatever repository the test was started from.
        let conn = spawn(cfg, home.clone(), tx);

        // A turn is a round trip to the service, so this is generous.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);

        // The catalogue arrives with the handshake, before any prompt, and
        // reports the mode the CLI actually started in — which is how the pick
        // is proved to have reached it.
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match rx.recv_timeout(remaining).expect("catalogue timed out") {
                Event::SessionOptions {
                    models,
                    modes,
                    current_mode,
                    access,
                    current_access,
                    reasoning,
                    sandboxes,
                    ..
                } => {
                    assert!(!models.is_empty(), "claude should offer models: {models:?}");
                    assert!(
                        !reasoning.is_empty(),
                        "claude should offer effort levels: {reasoning:?}"
                    );
                    assert!(
                        sandboxes.is_empty(),
                        "this backend offers no sandbox axis: {sandboxes:?}"
                    );
                    assert!(
                        access.iter().any(|policy| policy.value == "acceptEdits"),
                        "the access menu should offer the approval ladder: {access:?}"
                    );
                    assert!(
                        modes.iter().any(|mode| mode.value == "plan"),
                        "the work modes should offer planning: {modes:?}"
                    );
                    assert_eq!(
                        current_mode.as_deref(),
                        Some("plan"),
                        "the work mode kumu launched with is the one in force"
                    );
                    assert_eq!(
                        current_access, None,
                        "plan mode governs on its own, so no access policy is in force"
                    );
                    break;
                }
                Event::Error(error) => {
                    println!("skipping session_prompts_claude_end_to_end: {error}");
                    return;
                }
                _ => {}
            }
        }

        // A pick made now takes effect mid-session, through the control protocol
        // rather than the command line — and because one value backs both axes,
        // it also takes the session out of plan. The supervised policy is the one
        // that keeps the coming Write behind a permission prompt.
        conn.send(Cmd::SetMode("default".to_string()));

        // Asking for a file to be written is what makes the CLI ask *kumu*: a
        // fresh configuration approves nothing, and the session was launched
        // with the host as the answerer.
        conn.send(Cmd::Prompt(
            "Create a file named probe.txt in the current directory containing exactly: ok"
                .to_string(),
        ));

        let mut text = String::new();
        let mut asked = false;
        let mut mode_applied: Option<String> = None;
        let mut access_applied: Option<String> = None;
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match rx.recv_timeout(remaining).expect("prompt timed out") {
                Event::Agent(chunk) => text.push_str(&chunk),
                // Nobody is here to click, so take the first allow option and
                // let the turn finish; this is the answer path.
                Event::PermissionRequest {
                    request_id,
                    options,
                    ..
                } => {
                    asked = true;
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
                // The mid-session change is reported by re-emitting the
                // catalogue, with both projections moved.
                Event::SessionOptions {
                    current_mode,
                    current_access,
                    ..
                } => {
                    mode_applied = current_mode;
                    access_applied = current_access;
                }
                Event::Done(_) => {
                    println!("claude replied: {text:?}");
                    assert!(
                        asked,
                        "a Write call should have asked the host for permission"
                    );
                    assert_eq!(
                        mode_applied.as_deref(),
                        Some("default"),
                        "leaving plan is the normal working mode"
                    );
                    assert_eq!(
                        access_applied.as_deref(),
                        Some("default"),
                        "the access chip reads the same value the work chip does"
                    );
                    assert!(
                        home.join("probe.txt").exists(),
                        "the call the host allowed should have run"
                    );
                    break;
                }
                Event::Error(error) => panic!("claude session error: {error}"),
                _ => {}
            }
        }

        std::fs::remove_dir_all(&home).ok();
    }
}
