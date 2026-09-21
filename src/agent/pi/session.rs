//! The Pi session loop: one `pi --mode rpc` process per session, driven over
//! newline-delimited JSON.
//!
//! Same contract as the other backends: it produces [`Event`]s, consumes
//! [`Cmd`]s, and never names a protocol above `backend`. Pi asks for no
//! approvals, so the shared permission table is not used here — it runs with
//! `--approve` and has no permission system to ask with.
//!
//! Four things about this protocol shape the code below:
//!
//!   - **A reply is an acceptance, not a result.** `prompt` is acknowledged
//!     when Pi takes it, long before a model call has run. The turn that
//!     follows is followed on the event stream instead.
//!   - **`agent_end` is not the end of a turn.** One prompt can span several
//!     turns — a tool call ends one and its continuation begins another — and
//!     `agent_end` closes only the current low-level run, possibly to be
//!     followed by an automatic retry. Only `agent_settled` means nothing else
//!     will run, so that is the single event that settles a turn here.
//!   - **A failed run is not an error event.** A model call that cannot run
//!     finishes the assistant message with `stopReason: "error"` and the
//!     provider's own words in `errorMessage`, then settles normally — there is
//!     no error frame to catch. Checked against two providers that both refused
//!     a turn; see `schema/pi-rpc/0.85.1/`.
//!   - **Which frames carry a tool's arguments is asymmetric.**
//!     `tool_execution_start` has them, `_update` and `_end` do not, so the
//!     title is only ever read where it exists — which is exactly what
//!     [`Event::ToolCall`]'s "empty means untouched" contract wants.
//!
//! [`Event`]: crate::agent::Event
//! [`Cmd`]: crate::agent::Cmd

use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::FutureExt;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::convert;
use super::transport::{self, Transport, debug_log};
use crate::agent::config::Config;
use crate::agent::connection::WeakPermissions;
use crate::agent::event::{Cmd, Event, ModelInfo, ReasoningInfo, emit};
use crate::agent::session::{
    CANCELLED, CancelRequests, SessionOptionsBuilder, forget_stale_cancels, settle,
};

/// How long a prompt may stay unacknowledged as a *run* before it is treated as
/// one Pi answered itself.
///
/// Pi handles some messages without invoking the agent at all, and those open no
/// run and settle nothing — so waiting only for `agent_settled` would park the
/// turn, and with it the composer, which refuses to send while one is open.
/// `agent_start` is emitted before the first model call, so once it has arrived
/// the wait becomes unbounded: a turn may legitimately run for minutes, and a
/// timeout on it would cut off real work.
const FIRST_SIGNAL_TIMEOUT: Duration = Duration::from_secs(10);

/// Reported when Pi exits while a turn is still open.
const EXITED: &str = "pi 已退出";

/// What the message pump tells the command loop about the turn it opened.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Signal {
    /// A run began: the prompt was accepted and starts the agent.
    Started,
    /// Nothing else will run for this prompt.
    Settled,
    /// The agent exited with this turn still open.
    Exited,
}

/// The state of the turn in flight, reset when a prompt is sent.
#[derive(Default)]
struct PiStream {
    /// Whether the agent has begun the run for this prompt. Set by the first
    /// `agent_start`, and never cleared until the turn ends, because one prompt
    /// spans several turns.
    run_started: bool,
    /// Whether text (or reasoning) has already arrived as deltas for the
    /// current message, so the completed message is not added on top of them.
    saw_text: bool,
    saw_reasoning: bool,
    /// The reason this run failed, if it did. Reported when the turn settles,
    /// not when it happens: a failure Pi recovers from with a retry is not the
    /// turn's outcome, and kumu's `Event::Error` would end the turn early.
    failure: Option<String>,
}

/// The catalogue, and what is in force on each axis.
struct Options {
    models: Vec<ModelInfo>,
    current_model: Option<String>,
    reasoning: Vec<ReasoningInfo>,
    current_reasoning: Option<String>,
}

/// What the pump and the command loop share.
struct Shared {
    /// The registry id, stamped on every event this session emits.
    agent: &'static str,
    transport: Arc<Transport>,
    ev_tx: Sender<Event>,
    /// The turn in flight, so the pump can reach the loop that opened it.
    /// `None` when no turn is open, which is also what tells the pump that an
    /// exit has nothing left to settle.
    turn: Mutex<Option<mpsc::UnboundedSender<Signal>>>,
    stream: Mutex<PiStream>,
    options: Mutex<Options>,
}

/// What the pump does with a frame, beyond the events it already emitted.
#[derive(Default)]
struct Reaction {
    /// A line to write back (an answer to an extension dialog).
    reply: Option<Value>,
    /// A run began.
    started: bool,
    /// Nothing else will run for this prompt.
    settled: bool,
}

pub(in crate::agent) async fn run(
    cfg: Config,
    cwd: PathBuf,
    ev_tx: Sender<Event>,
    mut cmd_rx: mpsc::UnboundedReceiver<Cmd>,
    mut cancels: CancelRequests,
    _permissions: WeakPermissions,
) -> Result<(), String> {
    let (transport, mut messages, _child) =
        transport::spawn(&cfg.command, &cfg.args, &cfg.env, &cwd)
            .map_err(|error| format!("failed to launch {}: {error}", cfg.command))?;
    let transport = Arc::new(transport);

    if let Some(path) = &cfg.session.resume_id {
        // Pi opens a missing path as a new session, so check before switching.
        if !std::path::Path::new(path).is_file() {
            return Err(format!(
                "恢复 Pi 会话失败，会话文件不存在：{path}。未开启新会话。"
            ));
        }
        let response = transport
            .request(json!({"type": "switch_session", "sessionPath": path}))
            .await
            .map_err(|error| format!("恢复 Pi 会话失败，未开启新会话：{error}"))?;
        if response.pointer("/data/cancelled").and_then(Value::as_bool) == Some(true) {
            return Err("恢复 Pi 会话被取消，未发送消息。".into());
        }
    }

    let shared = Arc::new(Shared {
        agent: cfg.agent,
        transport: transport.clone(),
        ev_tx: ev_tx.clone(),
        turn: Mutex::new(None),
        stream: Mutex::new(PiStream::default()),
        options: Mutex::new(Options {
            // Until the session reports otherwise there is nothing to offer:
            // unlike Claude Code, Pi has a discovery request, so no fixed
            // vocabulary stands in for it.
            models: Vec::new(),
            current_model: None,
            reasoning: Vec::new(),
            current_reasoning: None,
        }),
    });

    // The picks a session was opened with go on before it reports, so the first
    // report is the state the user asked for rather than Pi's own default. Both
    // are best-effort: a pick that no longer resolves is worth saying so, but it
    // is not a reason to refuse the session.
    if let Some(model) = cfg.session.model.as_deref() {
        match convert::parse_model_slug(model) {
            Ok((provider, model_id)) => {
                let request =
                    json!({"type": "set_model", "provider": provider, "modelId": model_id});
                if let Err(error) = transport.request(request).await {
                    emit(&ev_tx, Event::Log(format!("应用会话模型失败：{error}")));
                }
            }
            Err(error) => emit(&ev_tx, Event::Error(error)),
        }
    }
    if let Some(level) = cfg.session.reasoning.as_deref() {
        let request = json!({"type": "set_thinking_level", "level": level});
        if let Err(error) = transport.request(request).await {
            emit(&ev_tx, Event::Log(format!("应用会话思考等级失败：{error}")));
        }
    }
    // A session that cannot describe its own state is not usable, so this one is
    // fatal: the connection reports it instead of exposing an agent whose model
    // and thinking level are unknown.
    refresh_options(&shared).await?;

    // The pump owns Pi's side: the event stream, and the answer to any extension
    // dialog that blocks it.
    let mut pump = {
        let shared = shared.clone();
        tokio::spawn(async move {
            while let Some(frame) = messages.recv().await {
                let reaction = {
                    let mut stream = shared.stream.lock().unwrap();
                    handle_frame(frame, &mut stream, &shared.ev_tx)
                };
                if let Some(reply) = reaction.reply {
                    shared.transport.send(reply);
                }
                let signal = match (reaction.started, reaction.settled) {
                    (_, true) => Some(Signal::Settled),
                    (true, false) => Some(Signal::Started),
                    _ => None,
                };
                if let Some(signal) = signal {
                    // A send that fails means the turn already ended; the signal
                    // is then moot rather than lost.
                    if let Some(sender) = shared.turn.lock().unwrap().as_ref() {
                        let _ = sender.send(signal);
                    }
                }
            }
            // The reader's channel closes when Pi's stdout ends, so reaching
            // here means the agent is gone. A turn still open will never be
            // settled by an event, so it is settled now — and told why, since
            // nothing else can tell it the agent exited.
            if let Some(sender) = shared.turn.lock().unwrap().take() {
                let _ = sender.send(Signal::Exited);
            }
        })
    };

    // Headless convenience, matching the other backends: one prompt before the
    // UI loop starts.
    if let Some(auto) = cfg.auto_prompt.clone() {
        emit(&ev_tx, Event::Log(format!("auto prompt: {auto}")));
        prompt(&shared, &auto, &mut cancels).await;
    }

    loop {
        // Two things can end this connection: the UI dropping its handle, or Pi
        // itself going away. The second is why the pump is raced here — a loop
        // parked on the command channel alone would keep the event channel open,
        // and the UI would go on believing it could still prompt a dead agent.
        futures::select! {
            command = cmd_rx.recv().fuse() => match command {
                Some(Cmd::Prompt(text)) => prompt(&shared, &text, &mut cancels).await,
                Some(Cmd::SetModel(value)) => set_model(&shared, &value).await,
                Some(Cmd::SetReasoning(value)) => set_reasoning(&shared, &value).await,
                // The composer only offers the axes an agent reports, and Pi
                // reports neither of these, so reaching here means a stored pick
                // outlived the agent that had the axis it belonged to.
                Some(Cmd::SetMode(value)) => {
                    emit(&ev_tx, Event::Log(format!("pi 没有可设置的 mode：{value}")));
                }
                Some(Cmd::SetSandbox(value)) => {
                    emit(&ev_tx, Event::Log(format!("pi 没有可设置的 sandbox：{value}")));
                }
                // Every UI-side handle was dropped.
                None => break,
            },
            _ = (&mut pump).fuse() => break,
        }
    }

    emit(&ev_tx, Event::Log("connection closing".to_string()));
    Ok(())
}

/// Send a prompt and follow it to the end of its turn.
async fn prompt(shared: &Arc<Shared>, text: &str, cancels: &mut CancelRequests) {
    forget_stale_cancels(cancels);
    *shared.stream.lock().unwrap() = PiStream::default();
    let (signal_tx, mut signal_rx) = mpsc::unbounded_channel::<Signal>();
    *shared.turn.lock().unwrap() = Some(signal_tx);

    let request = json!({"type": "prompt", "message": text});
    if let Err(error) = shared.transport.request(request).await {
        // Refused before Pi took it, so no run was opened and no event is
        // coming: nothing to wait for, and the reason is worth showing.
        *shared.turn.lock().unwrap() = None;
        for event in settle_events(Some(error), false, false) {
            emit(&shared.ev_tx, event);
        }
        return;
    }

    let mut started = false;
    let mut exited = false;
    let mut cancelled = false;
    loop {
        // A cancel is watched for on both waits: hitting pause before the run
        // has even started is just as valid as doing it mid-stream.
        let next = if started {
            tokio::select! {
                // A run is in flight, so the wait is unbounded: a turn may
                // legitimately run for minutes, and only its settle ends it.
                next = signal_rx.recv() => next,
                _ = cancels.recv(), if !cancelled => {
                    cancelled = true;
                    abort(shared).await;
                    continue;
                }
            }
        } else {
            tokio::select! {
                next = tokio::time::timeout(FIRST_SIGNAL_TIMEOUT, signal_rx.recv()) => match next {
                    Ok(next) => next,
                    // Nothing ever started, so nothing is coming: Pi answered
                    // this prompt itself and the turn is over.
                    Err(_) => break,
                },
                _ = cancels.recv(), if !cancelled => {
                    cancelled = true;
                    abort(shared).await;
                    continue;
                }
            }
        };
        match next {
            Some(Signal::Started) => started = true,
            Some(Signal::Settled) => break,
            Some(Signal::Exited) => {
                exited = true;
                break;
            }
            // The pump is gone and will not signal again.
            None => break,
        }
    }
    // Closed before the outcome is read: a later exit must not be reported
    // against a turn that has already ended.
    *shared.turn.lock().unwrap() = None;
    let failure = shared.stream.lock().unwrap().failure.clone();

    for event in settle_events(failure, exited, cancelled) {
        emit(&shared.ev_tx, event);
    }
}

/// Ask Pi to abort the run in flight.
///
/// Pi documents `abort` as waiting for the session to become idle before it
/// answers, so the reply is itself a synchronisation point — but the turn is
/// still followed on the event stream afterwards, because that is what settles
/// it here.
///
/// The documented recipe for an Esc key is `clear_queue` and then `abort`, so
/// that queued steering and follow-up messages are not continued after the
/// abort. kumu never queues one — the composer refuses to prompt while a turn is
/// open — so there is nothing to clear and `clear_queue` is left out.
async fn abort(shared: &Arc<Shared>) {
    emit(&shared.ev_tx, Event::Log("cancel requested".into()));
    if let Err(error) = shared.transport.request(json!({"type": "abort"})).await {
        emit(&shared.ev_tx, Event::Log(format!("abort 失败：{error}")));
    }
}

/// What a settled turn reports: Pi's own stop vocabulary (a failure settles as
/// `error`, a clean run as `stop`), in the order the shared contract requires.
/// A cancelled run is reported as [`CANCELLED`] rather than as either, so the UI
/// can say the turn was stopped rather than finished.
fn settle_events(failure: Option<String>, exited: bool, cancelled: bool) -> Vec<Event> {
    let failure = if exited {
        Some(EXITED.to_string())
    } else {
        failure
    };
    let stop = if cancelled {
        CANCELLED
    } else if failure.is_some() {
        "error"
    } else {
        "stop"
    };
    settle(stop, failure)
}

/// Switch the model in session, then re-read what the session reports.
///
/// Both axes are re-read because the thinking levels belong to the selected
/// model: Pi clamps a level the new model does not honour, and a menu that kept
/// the old model's levels would offer one Pi would refuse.
async fn set_model(shared: &Arc<Shared>, value: &str) {
    let (provider, model_id) = match convert::parse_model_slug(value) {
        Ok(parts) => parts,
        Err(error) => {
            emit(&shared.ev_tx, Event::Error(error));
            return;
        }
    };
    if shared.options.lock().unwrap().current_model.as_deref() == Some(value) {
        return;
    }
    let request = json!({"type": "set_model", "provider": provider, "modelId": model_id});
    match shared.transport.request(request).await {
        Ok(_) => {
            if let Err(error) = refresh_options(shared).await {
                emit(&shared.ev_tx, Event::Error(error));
            }
        }
        // Reported rather than logged: the user picked this model, so a refusal
        // is theirs to see — and the picker's stored value is now wrong.
        Err(error) => emit(
            &shared.ev_tx,
            Event::Error(format!("切换模型失败：{error}")),
        ),
    }
}

/// Switch the thinking level in session.
async fn set_reasoning(shared: &Arc<Shared>, value: &str) {
    if shared.options.lock().unwrap().current_reasoning.as_deref() == Some(value) {
        return;
    }
    let request = json!({"type": "set_thinking_level", "level": value});
    match shared.transport.request(request).await {
        Ok(_) => {
            if let Err(error) = refresh_options(shared).await {
                emit(&shared.ev_tx, Event::Error(error));
            }
        }
        Err(error) => emit(
            &shared.ev_tx,
            Event::Error(format!("切换思考等级失败：{error}")),
        ),
    }
}

/// Re-read the session's state, catalogue, thinking levels and commands, and
/// report them.
///
/// The four are one report because they move together: which model is in force
/// decides which levels exist, and a change of either usually means the session
/// was reconfigured. A request that fails keeps the last good answer rather than
/// emptying the menu — the session is still driven by commands either way, and a
/// picker that silently lost its models would look like the agent had stopped
/// offering any.
async fn refresh_options(shared: &Arc<Shared>) -> Result<(), String> {
    let state = shared
        .transport
        .request(json!({"type": "get_state"}))
        .await
        .map_err(|error| format!("读取 pi 会话状态失败：{error}"))?;
    let data = state.get("data").cloned().unwrap_or(Value::Null);
    if let Some(path) = data
        .get("sessionFile")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        emit(
            &shared.ev_tx,
            Event::SessionReady {
                resume_id: path.to_string(),
            },
        );
    }
    let models = shared
        .transport
        .request(json!({"type": "get_available_models"}))
        .await
        .ok()
        .map(|frame| convert::model_catalog(frame.get("data").unwrap_or(&Value::Null)));
    let levels = shared
        .transport
        .request(json!({"type": "get_available_thinking_levels"}))
        .await
        .ok()
        .map(|frame| convert::reasoning_catalog(frame.get("data").unwrap_or(&Value::Null)));
    // The commands this session will run. Asked for alongside the rest because
    // they move together — a skill loaded, an extension registered. A refused
    // request reports nothing at all, which leaves the menu holding the last
    // catalogue it was given rather than emptying it.
    let commands = shared
        .transport
        .request(json!({"type": "get_commands"}))
        .await
        .ok()
        .map(|frame| convert::command_catalog(frame.get("data").unwrap_or(&Value::Null)));

    let options = {
        let mut options = shared.options.lock().unwrap();
        if let Some(models) = models {
            options.models = models;
        }
        if let Some(levels) = levels {
            options.reasoning = levels;
        }
        options.current_model = convert::current_model(&data);
        options.current_reasoning = convert::current_reasoning(&data);
        // Pi has neither a mode selector nor a sandbox, and no permission system
        // to expose an access policy from: it runs with `--approve` and never
        // asks.
        SessionOptionsBuilder::new(shared.agent)
            .models(options.models.clone(), options.current_model.clone())
            .reasoning(options.reasoning.clone(), options.current_reasoning.clone())
            .build()
    };
    emit(&shared.ev_tx, options);
    if let Some(commands) = commands {
        emit(
            &shared.ev_tx,
            Event::Commands {
                agent: shared.agent,
                commands,
            },
        );
    }
    if let Some(title) = convert::session_title(&data) {
        emit(&shared.ev_tx, Event::SessionTitle { title });
    }
    Ok(())
}

/// Translate one frame into events, and report what it means for the turn.
///
/// Free-standing rather than a method so the mapping can be exercised without a
/// process: everything it needs is the frame, the turn's state and the event
/// sender.
fn handle_frame(frame: Value, stream: &mut PiStream, ev_tx: &Sender<Event>) -> Reaction {
    let mut reaction = Reaction::default();
    let frame_type = frame.get("type").and_then(Value::as_str).unwrap_or("");
    let assistant = is_assistant(&frame);

    match frame_type {
        // One prompt spans several runs — a tool call ends one turn and the
        // continuation begins another — but the user's turn was opened once, so
        // the run is announced once.
        "agent_start" | "turn_start" => {
            if !stream.run_started {
                stream.run_started = true;
                reaction.started = true;
            }
        }
        "message_start" if assistant => {
            stream.saw_text = false;
            stream.saw_reasoning = false;
        }
        "message_update" => {
            let update = frame.get("assistantMessageEvent").unwrap_or(&Value::Null);
            match update.get("type").and_then(Value::as_str) {
                Some("text_delta") => {
                    if let Some(delta) = non_empty(update.get("delta")) {
                        stream.saw_text = true;
                        emit(ev_tx, Event::Agent(delta));
                    }
                }
                Some("thinking_delta") => {
                    if let Some(delta) = non_empty(update.get("delta")) {
                        stream.saw_reasoning = true;
                        // Pi stamps no id on a streamed message.
                        emit(
                            ev_tx,
                            Event::Thought {
                                text: delta,
                                message: None,
                            },
                        );
                    }
                }
                // `toolcall_start`/`_delta`/`_end` are deliberately not turned
                // into rows: the `tool_execution_*` events carry the same call
                // with its arguments and its output, and rendering both would
                // put two rows on every tool call.
                _ => {}
            }
        }
        "message_end" if assistant => {
            // The completed message repeats what streamed. It is used only where
            // nothing did — a non-streaming provider, or a message that arrived
            // whole.
            let message = frame.get("message").cloned().unwrap_or(Value::Null);
            if !stream.saw_text
                && let Some(text) = convert::message_text(&message)
            {
                stream.saw_text = true;
                emit(ev_tx, Event::Agent(text));
            }
            if !stream.saw_reasoning
                && let Some(thinking) = convert::message_thinking(&message)
            {
                stream.saw_reasoning = true;
                emit(
                    ev_tx,
                    Event::Thought {
                        text: thinking,
                        message: None,
                    },
                );
            }
            note_failure(stream, convert::failure(&message), ev_tx);
        }
        "tool_execution_start" | "tool_execution_update" | "tool_execution_end" => {
            let Some(id) = non_empty(frame.get("toolCallId")) else {
                return reaction;
            };
            let name = frame.get("toolName").and_then(Value::as_str).unwrap_or("");
            let (title, status, content) = match frame_type {
                // The only frame carrying the arguments, and therefore the only
                // one that can name the call. The later frames send an empty
                // title so the row keeps the one it already has.
                "tool_execution_start" => (
                    convert::tool_title(name, frame.get("args").unwrap_or(&Value::Null)),
                    // The row is live until its end frame arrives, which is what
                    // distinguishes a running call from a finished one.
                    "running",
                    String::new(),
                ),
                // The update carries the output accumulated so far rather than a
                // delta, and says nothing about the call's status.
                "tool_execution_update" => (
                    String::new(),
                    "",
                    convert::tool_content(frame.get("partialResult").unwrap_or(&Value::Null)),
                ),
                _ => (
                    String::new(),
                    if frame.get("isError").and_then(Value::as_bool) == Some(true) {
                        "failed"
                    } else {
                        "done"
                    },
                    convert::tool_content(frame.get("result").unwrap_or(&Value::Null)),
                ),
            };
            emit(
                ev_tx,
                Event::ToolCall {
                    id,
                    title,
                    status,
                    content,
                },
            );
        }
        "auto_retry_end" => {
            if frame.get("success").and_then(Value::as_bool) == Some(true) {
                // The attempt that failed is not the turn's outcome: Pi
                // recovered, so nothing is reported from it.
                stream.failure = None;
            } else {
                // The retries are spent, so this string is the run's reason.
                note_failure(
                    stream,
                    Some(
                        non_empty(frame.get("finalError"))
                            .unwrap_or_else(|| "pi 的自动重试最终失败".to_string()),
                    ),
                    ev_tx,
                );
            }
        }
        "agent_settled" => reaction.settled = true,
        "session_info_changed" => {
            if let Some(title) = non_empty(frame.get("name")) {
                emit(ev_tx, Event::SessionTitle { title });
            }
        }
        "extension_ui_request" => {
            // A dialog method blocks the extension until the host answers, and
            // kumu has no surface for one — so cancelling is what keeps an
            // extension from parking the turn behind a prompt nobody will see.
            // The fire-and-forget methods (`notify`, `setStatus`, …) expect no
            // answer, and the frame is simply not acted on.
            let method = frame.get("method").and_then(Value::as_str);
            if matches!(method, Some("select" | "confirm" | "input" | "editor"))
                && let Some(id) = non_empty(frame.get("id"))
            {
                reaction.reply = Some(json!({
                    "type": "extension_ui_response",
                    "id": id,
                    "cancelled": true,
                }));
            }
        }
        "extension_error" => {
            // An extension threw. Pi carries on with the run, so this is not the
            // turn's failure — reporting it as one would end the turn in kumu
            // while the agent was still working.
            debug_log(&format!(
                "extension error: {}",
                frame.get("error").and_then(Value::as_str).unwrap_or("")
            ));
        }
        _ => {}
    }

    reaction
}

/// Record why a run failed, when the frame says it did.
///
/// One report per turn: a run can fail several times over as Pi retries, and
/// only the last reason is the one worth showing. It is logged as well as
/// remembered, so a headless run leaves a trace of a turn that ended badly.
fn note_failure(stream: &mut PiStream, detail: Option<String>, ev_tx: &Sender<Event>) {
    let Some(detail) = detail else {
        return;
    };
    emit(ev_tx, Event::Log(detail.clone()));
    stream.failure = Some(detail);
}

/// Whether a frame is about the assistant's own message rather than the user's.
fn is_assistant(frame: &Value) -> bool {
    frame.pointer("/message/role").and_then(Value::as_str) == Some("assistant")
}

/// A non-empty trimmed string from a JSON value.
fn non_empty(value: Option<&Value>) -> Option<String> {
    let value = value?.as_str()?.trim();
    (!value.is_empty()).then(|| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::test_support::{AGENT_TEST_LOCK, command_exists};
    use serde_json::json;
    use std::time::Instant;

    /// Drive frames through the mapping and collect what the UI would see.
    fn drive(frames: &[Value]) -> (Vec<Event>, PiStream, bool) {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut stream = PiStream::default();
        let mut settled = false;
        for frame in frames {
            let reaction = handle_frame(frame.clone(), &mut stream, &tx);
            settled |= reaction.settled;
        }
        let events = rx.try_iter().collect();
        (events, stream, settled)
    }

    fn kinds(events: &[Event]) -> Vec<&'static str> {
        events
            .iter()
            .map(|event| match event {
                Event::Log(_) => "log",
                Event::Agent(_) => "agent",
                Event::Thought { .. } => "thought",
                Event::ToolCall { .. } => "tool",
                Event::SessionTitle { .. } => "title",
                Event::SessionReady { .. } => "ready",
                Event::Done(_) => "done",
                Event::PermissionRequest { .. } => "permission",
                Event::Error(_) => "error",
                Event::SessionOptions { .. } => "options",
                Event::Commands { .. } => "commands",
                Event::ProbeFailed { .. } => "failed",
            })
            .collect()
    }

    /// A run streams its text and reasoning, reports each tool call once with
    /// the arguments it started with, and settles exactly once.
    #[test]
    fn a_run_streams_its_text_reasoning_and_tools_and_settles_once() {
        let (events, stream, settled) = drive(&[
            json!({"type": "agent_start"}),
            json!({"type": "turn_start"}),
            json!({
                "type": "message_update",
                "assistantMessageEvent": {"type": "thinking_delta", "delta": "weighing it"}
            }),
            json!({
                "type": "message_update",
                "assistantMessageEvent": {"type": "toolcall_start", "toolName": "read"}
            }),
            json!({
                "type": "tool_execution_start",
                "toolCallId": "tool-1",
                "toolName": "read",
                "args": {"path": "/etc/hosts"}
            }),
            json!({
                "type": "tool_execution_update",
                "toolCallId": "tool-1",
                "toolName": "read",
                "partialResult": {"content": []}
            }),
            json!({
                "type": "tool_execution_end",
                "toolCallId": "tool-1",
                "toolName": "read",
                "result": {"content": [{"type": "text", "text": "127.0.0.1 localhost"}]},
                "isError": false
            }),
            json!({
                "type": "message_update",
                "assistantMessageEvent": {"type": "text_delta", "delta": "ok"}
            }),
            // The completed message repeats the deltas; it must add nothing.
            json!({
                "type": "message_end",
                "message": {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "weighing it"},
                    {"type": "text", "text": "ok"}
                ], "stopReason": "stop"}
            }),
            json!({"type": "agent_end", "willRetry": false}),
            json!({"type": "agent_settled"}),
        ]);

        assert_eq!(
            kinds(&events),
            ["thought", "tool", "tool", "tool", "agent"],
            "one row per tool frame, deltas once, and the tool-call deltas ignored"
        );
        // The update carries no output, and the end carries the result.
        let Event::ToolCall {
            title,
            status,
            content,
            ..
        } = &events[1]
        else {
            panic!("expected the tool's first row");
        };
        assert_eq!(title, "read · /etc/hosts", "the start frame names the call");
        assert_eq!(*status, "running");
        assert_eq!(content, "");
        let Event::ToolCall {
            title,
            status,
            content,
            ..
        } = &events[3]
        else {
            panic!("expected the tool's completed row");
        };
        assert_eq!(title, "", "the end frame has no arguments to name it with");
        assert_eq!(*status, "done");
        assert!(content.contains("127.0.0.1"));
        assert!(stream.run_started);
        assert!(settled, "the settled event is what ends the turn");
        assert!(stream.failure.is_none());
    }

    /// A tool that fails ends as a failure rather than a success.
    #[test]
    fn a_failed_tool_call_ends_as_a_failure() {
        let (events, _, _) = drive(&[
            json!({"type": "tool_execution_start", "toolCallId": "t", "toolName": "bash", "args": {"command": "false"}}),
            json!({"type": "tool_execution_end", "toolCallId": "t", "toolName": "bash", "result": {"content": [{"type": "text", "text": "boom"}]}, "isError": true}),
        ]);
        let Event::ToolCall { status, .. } = &events[1] else {
            panic!("expected a completed row");
        };
        assert_eq!(*status, "failed");
    }

    /// A message that arrives without having streamed is shown from its
    /// completed form, and its provider signature never reaches the transcript.
    #[test]
    fn a_message_that_did_not_stream_is_shown_from_its_completion() {
        let (events, _, _) = drive(&[json!({
            "type": "message_end",
            "message": {"role": "assistant", "content": [
                {"type": "text", "text": "ok", "textSignature": "{\"v\":1,\"phase\":\"final_answer\"}"}
            ], "stopReason": "stop"}
        })]);
        assert_eq!(kinds(&events), ["agent"]);
        let Event::Agent(text) = &events[0] else {
            panic!("expected the message's text");
        };
        assert_eq!(text, "ok");
        assert!(!text.contains("textSignature"));
    }

    /// A refused turn is recorded, not reported: the run is still going, and an
    /// `Event::Error` here would end the turn in kumu while Pi worked on.
    #[test]
    fn a_failed_run_is_recorded_rather_than_reported_mid_turn() {
        let (events, stream, settled) = drive(&[
            json!({"type": "agent_start"}),
            json!({
                "type": "message_end",
                "message": {"role": "assistant", "content": [], "stopReason": "error",
                            "errorMessage": "Insufficient balance."}
            }),
            json!({"type": "agent_settled"}),
        ]);
        assert_eq!(
            kinds(&events),
            ["log"],
            "the reason is logged for KUMU_DEBUG, not raised as an error"
        );
        assert_eq!(stream.failure.as_deref(), Some("Insufficient balance."));
        assert!(settled);
    }

    /// What a settled turn reports, and in which order.
    #[test]
    fn settling_reports_the_failure_after_ending_the_turn() {
        // The turn ends first: the UI persists the blocks it streamed when it
        // ends, so an error ahead of it would discard them.
        assert_eq!(
            kinds(&settle_events(Some("boom".into()), false, false)),
            ["done", "error"]
        );
        assert_eq!(kinds(&settle_events(None, false, false)), ["done"]);
        assert_eq!(
            kinds(&settle_events(None, true, false)),
            ["done", "error"],
            "an agent that exited mid-turn failed the turn"
        );
        let Event::Error(detail) = &settle_events(None, true, false)[1] else {
            panic!("expected an error");
        };
        assert_eq!(detail, EXITED);
    }

    /// A cancelled run is reported with its own stop reason rather than as a
    /// failure, because the user asked for it. Pi's own word for it is
    /// `aborted`, which never reaches the UI: the backend maps it here.
    #[test]
    fn settling_a_cancelled_run_says_so_instead_of_failing() {
        let events = settle_events(None, false, true);
        let Event::Done(stop) = &events[0] else {
            panic!("expected a settle");
        };
        assert_eq!(stop, CANCELLED);
        assert!(
            !events.iter().any(|event| matches!(event, Event::Error(_))),
            "a cancel the user asked for is not a failure"
        );
    }

    /// A retry that succeeds clears the failure its earlier attempt recorded,
    /// and one that gives up keeps it.
    #[test]
    fn a_retry_replaces_the_failure_it_recovers_from() {
        let (_, stream, _) = drive(&[
            json!({"type": "auto_retry_end", "success": false, "finalError": "overloaded"}),
            json!({"type": "auto_retry_end", "success": true, "attempt": 2}),
        ]);
        assert!(stream.failure.is_none(), "a recovered run did not fail");

        let (_, stream, _) = drive(&[
            json!({"type": "auto_retry_end", "success": false, "finalError": "overloaded"}),
        ]);
        assert_eq!(stream.failure.as_deref(), Some("overloaded"));
    }

    /// An extension dialog is answered — cancelling is what keeps it from
    /// parking the turn — and a fire-and-forget method is left alone.
    #[test]
    fn extension_dialogs_are_cancelled_and_notifications_ignored() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut stream = PiStream::default();

        let dialog = handle_frame(
            json!({"type": "extension_ui_request", "id": "ui-1", "method": "confirm",
                   "title": "Continue?", "timeout": 30000}),
            &mut stream,
            &tx,
        );
        let reply = dialog.reply.expect("a dialog must be answered");
        assert_eq!(reply["type"], "extension_ui_response");
        assert_eq!(reply["id"], "ui-1");
        assert_eq!(reply["cancelled"], true);

        let passive = handle_frame(
            json!({"type": "extension_ui_request", "id": "ui-2", "method": "setStatus",
                   "statusKey": "usage", "statusText": "checking"}),
            &mut stream,
            &tx,
        );
        assert!(passive.reply.is_none(), "notifications expect no answer");

        assert!(
            rx.try_iter().next().is_none(),
            "an extension frame is not transcript content"
        );
    }

    /// A session's own name becomes its title, and the run is announced once per
    /// prompt however many turns it spans.
    #[test]
    fn titles_are_forwarded_and_the_run_is_announced_once() {
        let (events, _, _) = drive(&[
            json!({"type": "agent_start"}),
            json!({"type": "turn_start"}),
            json!({"type": "session_info_changed", "name": "fix the parser"}),
            json!({"type": "session_info_changed", "name": null}),
        ]);
        assert_eq!(
            kinds(&events),
            ["title"],
            "a cleared name announces nothing"
        );
        let Event::SessionTitle { title } = &events[0] else {
            panic!("expected a title");
        };
        assert_eq!(title, "fix the parser");

        let (tx, rx) = std::sync::mpsc::channel();
        let mut stream = PiStream::default();
        let first = handle_frame(json!({"type": "agent_start"}), &mut stream, &tx);
        let second = handle_frame(json!({"type": "agent_start"}), &mut stream, &tx);
        assert!(
            first.started && !second.started,
            "the run is announced once"
        );
        assert_eq!(rx.try_iter().count(), 0);
    }

    /// End-to-end: drive a real `pi` through two prompts.
    ///
    /// The assertions are about the *shape* of a turn rather than its content,
    /// because a turn can end without producing an answer — an expired key, a
    /// spent balance, a model the account cannot serve — and that is still a
    /// turn, which still has to settle. What this covers is the part that could
    /// hang: Pi acknowledges a prompt before it runs one, and only
    /// `agent_settled` ends it, so a prompt that never settles would leave the
    /// composer refusing to send for good. Two prompts in a row is what proves
    /// the loop re-arms.
    ///
    /// Ignored by default, so a plain `cargo test` never drives a real agent;
    /// `cargo test -- --ignored` runs it, and it skips itself when the binary is
    /// absent.
    #[test]
    #[ignore = "drives the real pi binary"]
    fn session_prompts_pi_end_to_end_twice() {
        let _guard = AGENT_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let cfg = Config::for_agent("pi").expect("pi is a registered agent");
        if !command_exists(&cfg.command) {
            println!(
                "skipping session_prompts_pi_end_to_end_twice: {} not found",
                cfg.command
            );
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        // The connection must outlive both prompts; dropping it is the teardown
        // signal.
        let conn = crate::agent::spawn(cfg, std::env::temp_dir(), tx);

        // The handshake is what fills the pickers, and what proves the RPC
        // session is live rather than merely spawned.
        let mut announced = false;
        let handshake_deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < handshake_deadline {
            let remaining = handshake_deadline.saturating_duration_since(Instant::now());
            match rx
                .recv_timeout(remaining)
                .expect("pi never reported its options")
            {
                Event::SessionOptions { models, .. } => {
                    assert!(!models.is_empty(), "pi reported no models");
                    announced = true;
                    break;
                }
                Event::Error(error) => panic!("pi failed to initialize: {error}"),
                _ => {}
            }
        }
        assert!(announced, "the handshake never reported a catalogue");

        for attempt in 1..=2 {
            conn.send(Cmd::Prompt(
                "Reply with exactly: ok. Do not use any tools.".into(),
            ));
            let mut streamed = String::new();
            let mut settled = false;
            let deadline = Instant::now() + Duration::from_secs(180);
            while Instant::now() < deadline {
                let remaining = deadline.saturating_duration_since(Instant::now());
                let Ok(event) = rx.recv_timeout(remaining) else {
                    break;
                };
                match event {
                    Event::Agent(chunk) => streamed.push_str(&chunk),
                    Event::Done(reason) => {
                        println!(
                            "prompt {attempt}: settled as {reason:?} after {} chars",
                            streamed.len()
                        );
                        settled = true;
                        break;
                    }
                    _ => {}
                }
            }
            // The point of the second prompt: the first turn's settle released
            // the loop, so this one is driven at all.
            assert!(settled, "prompt {attempt} never settled");
        }
    }
}
