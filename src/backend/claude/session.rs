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
    AccessInfo, Cmd, CommandInfo, Event, ModeInfo, ModelInfo, ReasoningInfo, SandboxInfo, emit,
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
    /// `"{message id}:text"` / `"{message id}:thinking"` for each kind of
    /// content that already arrived as deltas for that message — kept apart
    /// because the two do not always stream together: text reliably does, but
    /// a model may hand back a short or unstreamed thinking block for the same
    /// message, and marking the whole message "streamed" from text alone would
    /// wrongly swallow that thinking when the complete message repeats it.
    streamed: Mutex<HashSet<String>>,
    /// The message currently streaming. `message_start` is the only event that
    /// names it; every delta after that carries no id of its own and belongs to
    /// whichever message started most recently (see `ApiEvent::message_id`'s own
    /// doc comment).
    current_message: Mutex<Option<String>>,
    options: Mutex<Options>,
    /// The commands this session will run, as the handshake reported them. Held
    /// because the only other word on them — which of the names are terminal-only
    /// — arrives with the first turn, and the catalogue has to be re-reported
    /// without them then.
    commands: Mutex<Vec<CommandInfo>>,
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

    /// Report the commands this session will run.
    ///
    /// Sent after the handshake, and again when the first turn names the
    /// terminal-only ones. An invocation is a prompt like any other — the CLI
    /// recognizes its own `/` prefix — so this is a catalogue to choose from,
    /// not a mode the session is in.
    fn emit_commands(&self) {
        emit(
            &self.ev_tx,
            Event::Commands {
                agent: self.agent,
                commands: self.commands.lock().unwrap().clone(),
            },
        );
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
        current_message: Mutex::new(None),
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
        // The handshake's catalogue. Nothing can be terminal-only yet: the CLI
        // only says which names are on the first turn, so every command it lists
        // is offered until `system/init` says otherwise.
        commands: Mutex::new(convert::command_catalog(&reported.commands, &[])),
        launched: cfg.session.clone(),
        cancelled: AtomicBool::new(false),
    });
    shared.emit_options();
    shared.emit_commands();

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
            // The first turn is also where the CLI names the commands that only
            // run in an interactive terminal. Sending one over this wire does
            // nothing, so they are dropped from the catalogue the composer
            // offers rather than left there as an entry that cannot act.
            if !system.terminal_slash_commands.is_empty() {
                let terminal = system.terminal_slash_commands;
                let mut commands = shared.commands.lock().unwrap();
                let before = commands.len();
                commands.retain(|command| !terminal.iter().any(|name| name == &command.name));
                let changed = commands.len() != before;
                drop(commands);
                if changed {
                    shared.emit_commands();
                }
            }
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
    // Checked per kind, not once for the whole message: text reliably streams,
    // but a model may hand back thinking that never did (or the reverse), and
    // the message's other kind having streamed must not swallow the one that
    // did not.
    let streamed = |kind: &str| {
        !id.is_empty()
            && shared
                .streamed
                .lock()
                .unwrap()
                .contains(&format!("{id}:{kind}"))
    };
    // A message whose content is a bare string is text without blocks; the CLI
    // sends that shape for some of its own replies.
    if let protocol::ContentBlocks::Text(text) = &message.message.content {
        if !streamed("text") && !text.is_empty() {
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
            ContentBlock::Text { text } if !streamed("text") => {
                emit(&shared.ev_tx, Event::Agent(text.clone()))
            }
            ContentBlock::Thinking { thinking } if !streamed("thinking") => {
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
        // Only `message_start` names its message; every delta after that
        // carries no id of its own, so this is remembered for them.
        *shared.current_message.lock().unwrap() = Some(id.to_string());
    }
    if event.kind != "content_block_delta" {
        return;
    }
    // A delta with nothing to attribute it to (no `message_start` seen yet)
    // has nothing to mark as streamed, but the text or thinking is still worth
    // showing.
    let id = shared.current_message.lock().unwrap().clone();
    match event.delta.kind.as_str() {
        "text_delta" => {
            if let Some(text) = &event.delta.text {
                if let Some(id) = &id {
                    shared.streamed.lock().unwrap().insert(format!("{id}:text"));
                }
                emit(&shared.ev_tx, Event::Agent(text.clone()));
            }
        }
        "thinking_delta" => {
            if let Some(thinking) = &event.delta.thinking {
                if let Some(id) = &id {
                    shared
                        .streamed
                        .lock()
                        .unwrap()
                        .insert(format!("{id}:thinking"));
                }
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

    use super::{Awaiting, CommandInfo, Message, Options, Shared, Transport, handle_message};
    use serde_json::{Value, json};
    use std::collections::{HashSet, VecDeque};
    use std::sync::atomic::{AtomicBool, AtomicU64};
    use std::sync::{Arc, Mutex, Weak};

    /// A `Shared` wired to nowhere — a detached transport nothing reads, no
    /// permissions table — plus the receiving half of its event channel, for
    /// tests that drive the message handlers directly instead of a real `claude`
    /// process. Mirrors the Pi backend's synthetic-stream tests, adapted to the
    /// state this backend's handlers read from.
    fn shared_for_test() -> (Shared, std::sync::mpsc::Receiver<Event>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let shared = Shared {
            agent: "claude",
            transport: Arc::new(Transport::detached_for_test()),
            ev_tx: tx,
            permissions: Weak::new(),
            next_request_id: AtomicU64::new(1),
            awaiting: Mutex::new(VecDeque::new()),
            turn_done: Mutex::new(None),
            streamed: Mutex::new(HashSet::new()),
            current_message: Mutex::new(None),
            options: Mutex::new(Options {
                models: Vec::new(),
                current_model: None,
                modes: Vec::new(),
                current_mode: None,
                access: Vec::new(),
                current_access: None,
                sandboxes: Vec::new(),
                current_sandbox: None,
                reasoning: Vec::new(),
                current_reasoning: None,
            }),
            commands: Mutex::new(Vec::new()),
            launched: SessionPicks::default(),
            cancelled: AtomicBool::new(false),
        };
        (shared, rx)
    }

    /// Feed raw stream lines through the same frame handler the pump calls.
    fn drive(shared: &Arc<Shared>, lines: &[Value]) {
        for line in lines {
            handle_message(shared, Message::Line(line.clone()));
        }
    }

    fn agent_chunks(events: &[Event]) -> Vec<&str> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::Agent(t) => Some(t.as_str()),
                _ => None,
            })
            .collect()
    }

    /// `Event` carries no `Debug` impl, so a failing assertion names what
    /// actually came through by kind instead — same purpose as the Pi
    /// backend's own `kinds` helper.
    fn kinds(events: &[Event]) -> Vec<&'static str> {
        events
            .iter()
            .map(|e| match e {
                Event::Log(_) => "log",
                Event::Agent(_) => "agent",
                Event::Thought(_) => "thought",
                Event::ToolCall { .. } => "tool",
                Event::SessionTitle { .. } => "title",
                Event::Done(_) => "done",
                Event::PermissionRequest { .. } => "permission",
                Event::Error(_) => "error",
                Event::SessionOptions { .. } => "options",
                Event::Commands { .. } => "commands",
                Event::ProbeFailed { .. } => "failed",
            })
            .collect()
    }

    /// Text that already streamed as deltas must not be repeated when its
    /// `assistant` twin arrives — the CLI sends the complete blocks again once
    /// the message settles.
    #[test]
    fn streamed_text_is_not_repeated_by_its_assistant_twin() {
        let (shared, rx) = shared_for_test();
        let shared = Arc::new(shared);
        drive(
            &shared,
            &[
                json!({
                    "type": "stream_event",
                    "event": {
                        "type": "content_block_delta",
                        "message": {"id": "m1"},
                        "delta": {"type": "text_delta", "text": "hi"},
                    },
                }),
                json!({
                    "type": "assistant",
                    "message": {"id": "m1", "content": [{"type": "text", "text": "hi"}]},
                }),
            ],
        );
        assert_eq!(
            agent_chunks(&rx.try_iter().collect::<Vec<_>>()),
            vec!["hi"],
            "the assistant twin repeats what already streamed and must be skipped"
        );
    }

    /// A message that never streamed — the CLI's own reply to a slash command,
    /// say — has no delta to have shown it, so its `assistant` line is the only
    /// chance to show the text.
    #[test]
    fn an_assistant_message_that_never_streamed_shows_its_text() {
        let (shared, rx) = shared_for_test();
        let shared = Arc::new(shared);
        drive(
            &shared,
            &[json!({
                "type": "assistant",
                "message": {"id": "m2", "content": [{"type": "text", "text": "switched model"}]},
            })],
        );
        assert_eq!(
            agent_chunks(&rx.try_iter().collect::<Vec<_>>()),
            vec!["switched model"]
        );
    }

    /// Text and thinking are two different kinds of content on the same
    /// message, and do not always stream together: a model can hand back text
    /// deltas without ever sending a `thinking_delta` for the same reply.
    /// Marking the whole message "streamed" from the text alone must not
    /// swallow the thinking when the complete message repeats both.
    #[test]
    fn thinking_that_never_streamed_still_shows_when_only_text_did() {
        let (shared, rx) = shared_for_test();
        let shared = Arc::new(shared);
        drive(
            &shared,
            &[
                json!({
                    "type": "stream_event",
                    "event": {"type": "message_start", "message": {"id": "m1"}},
                }),
                json!({
                    "type": "stream_event",
                    "event": {
                        "type": "content_block_delta",
                        "delta": {"type": "text_delta", "text": "hi"},
                    },
                }),
                json!({
                    "type": "assistant",
                    "message": {"id": "m1", "content": [
                        {"type": "thinking", "thinking": "reasoning..."},
                        {"type": "text", "text": "hi"},
                    ]},
                }),
            ],
        );
        let events: Vec<Event> = rx.try_iter().collect();
        assert_eq!(
            agent_chunks(&events),
            vec!["hi"],
            "the assistant twin's text already streamed and must not repeat"
        );
        let thoughts: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                Event::Thought(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            thoughts,
            vec!["reasoning..."],
            "thinking that never streamed must still show when the complete message arrives: {:?}",
            kinds(&events)
        );
    }

    /// A command sent as a message (`/model`, `/effort`) is queued as a command
    /// rather than a prompt, so its `result` must not end a turn — only log a
    /// failure when the command itself did not take.
    #[test]
    fn a_commands_result_does_not_end_a_turn() {
        let (shared, rx) = shared_for_test();
        shared.awaiting.lock().unwrap().push_back(Awaiting::Command);
        let shared = Arc::new(shared);
        drive(
            &shared,
            &[json!({"type": "result", "subtype": "success", "is_error": false})],
        );
        let events: Vec<Event> = rx.try_iter().collect();
        assert!(
            !events.iter().any(|e| matches!(e, Event::Done(_))),
            "a command's result must not settle the turn: {:?}",
            kinds(&events)
        );
    }

    /// A prompt's `result` is where the turn ends, carrying the CLI's stop
    /// reason.
    #[test]
    fn a_prompts_result_settles_the_turn() {
        let (shared, rx) = shared_for_test();
        shared.awaiting.lock().unwrap().push_back(Awaiting::Prompt);
        let shared = Arc::new(shared);
        drive(
            &shared,
            &[json!({
                "type": "result",
                "subtype": "success",
                "is_error": false,
                "stop_reason": "end_turn",
                "permission_denials": [],
            })],
        );
        let events: Vec<Event> = rx.try_iter().collect();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Done(stop) if stop == "end_turn")),
            "expected a Done(\"end_turn\"): {:?}",
            kinds(&events)
        );
    }

    /// An interrupted turn is reported as cancelled on kumu's own say-so, not
    /// whatever the CLI's `result` line happens to call it — the CLI's wording
    /// for an interruption is undocumented and was never captured.
    #[test]
    fn an_interrupted_turn_reports_cancelled_not_the_clis_own_reason() {
        let (shared, rx) = shared_for_test();
        shared.awaiting.lock().unwrap().push_back(Awaiting::Prompt);
        shared
            .cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let shared = Arc::new(shared);
        drive(
            &shared,
            &[json!({
                "type": "result",
                "subtype": "error_during_execution",
                "is_error": true,
                "result": "whatever the CLI calls an interruption",
                "permission_denials": [],
            })],
        );
        let events: Vec<Event> = rx.try_iter().collect();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Done(stop) if stop == "cancelled")),
            "expected a Done(\"cancelled\"): {:?}",
            kinds(&events)
        );
        assert!(
            !events.iter().any(|e| matches!(e, Event::Error(_))),
            "the user asked for this, so it is not shown as a failure: {:?}",
            kinds(&events)
        );
    }

    /// `system/init` reports the model the session resolved to and projects the
    /// one permission mode onto both the work-mode and access-policy chips.
    #[test]
    fn system_init_reports_the_resolved_model_and_permission_mode() {
        let (shared, rx) = shared_for_test();
        let shared = Arc::new(shared);
        drive(
            &shared,
            &[json!({
                "type": "system",
                "subtype": "init",
                "model": "opus",
                "permissionMode": "default",
            })],
        );
        let events: Vec<Event> = rx.try_iter().collect();
        let options = events
            .iter()
            .find_map(|e| match e {
                Event::SessionOptions {
                    current_model,
                    current_mode,
                    current_access,
                    ..
                } => Some((current_model, current_mode, current_access)),
                _ => None,
            })
            .expect("system/init should emit SessionOptions");
        assert_eq!(options.0.as_deref(), Some("opus"));
        assert_eq!(options.1.as_deref(), Some("default"));
        assert_eq!(options.2.as_deref(), Some("default"));
    }

    /// The handshake lists every command the CLI will run. The first turn then
    /// names the ones that only run in an interactive terminal, and those are
    /// re-reported without them: sending one over this wire does nothing, so a
    /// menu offering it would be offering an entry that cannot act.
    #[test]
    fn terminal_only_commands_are_dropped_when_the_first_turn_names_them() {
        let (shared, rx) = shared_for_test();
        *shared.commands.lock().unwrap() = vec![
            CommandInfo {
                name: "compact".into(),
                hint: None,
            },
            CommandInfo {
                name: "doctor".into(),
                hint: None,
            },
        ];
        let shared = Arc::new(shared);
        drive(
            &shared,
            &[json!({
                "type": "system",
                "subtype": "init",
                "permissionMode": "default",
                "terminal_slash_commands": ["doctor"],
            })],
        );
        let commands = rx
            .try_iter()
            .find_map(|event| match event {
                Event::Commands { commands, .. } => Some(commands),
                _ => None,
            })
            .expect("the first turn should re-report the catalogue");
        assert_eq!(
            commands
                .iter()
                .map(|command| command.name.as_str())
                .collect::<Vec<_>>(),
            ["compact"],
            "the terminal-only command is not offered"
        );
    }

    /// A tool call is two lines: the `assistant` half starts it running, the
    /// `user` half — the tool result — settles it as done or failed depending on
    /// the CLI's own `is_error` flag.
    #[test]
    fn a_tool_calls_two_halves_report_running_then_its_outcome() {
        let (shared, rx) = shared_for_test();
        let shared = Arc::new(shared);
        drive(
            &shared,
            &[
                json!({
                    "type": "assistant",
                    "message": {"id": "m3", "content": [
                        {"type": "tool_use", "id": "call_1", "name": "Write", "input": {"file_path": "a.txt"}},
                    ]},
                }),
                json!({
                    "type": "user",
                    "message": {"content": [
                        {"type": "tool_result", "tool_use_id": "call_1", "is_error": true, "content": "denied"},
                    ]},
                }),
            ],
        );
        let events: Vec<Event> = rx.try_iter().collect();
        let statuses: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                Event::ToolCall { id, status, .. } if id == "call_1" => Some(*status),
                _ => None,
            })
            .collect();
        assert_eq!(statuses, vec!["running", "failed"]);
    }

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
                    // An unauthenticated CLI answers in-band rather than with
                    // an `Event::Error` — this is the other half of "a CLI with
                    // no credentials says so instead of answering", which the
                    // `Event::Error` arm above only catches when the CLI fails
                    // outright instead of replying with the same news.
                    if text.to_lowercase().contains("not logged in") {
                        println!("skipping session_prompts_claude_end_to_end: not logged in");
                        std::fs::remove_dir_all(&home).ok();
                        return;
                    }
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
