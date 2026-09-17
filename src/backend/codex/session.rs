//! The Codex session: handshake, one thread, and the command loop.
//!
//! Deliberately shaped like the ACP backend — one session per connection,
//! serial prompts, approvals answered out of band through the shared permission
//! table — so which agent is talking stays invisible to the UI.
//!
//! One difference in kind: app-server multiplexes threads over a single
//! process, but kumu still runs one process per session. That is what keeps
//! "dropping the last `Connection` reclaims the agent" true.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use crate::backend::config::Config;
use crate::backend::connection::WeakPermissions;
use crate::backend::event::{Cmd, Event, ModelInfo, emit};
use crate::backend::session::{
    CANCELLED, CancelRequests, PermissionQuestion, SessionOptionsBuilder, ask_user,
    forget_stale_cancels, release_pending_permissions, settle,
};

use super::convert::{self, BuiltinCommand};
use super::protocol::{
    AccountResult, CommandApprovalParams, DeltaNotification, ErrorNotification,
    FileChangeApprovalParams, InitializeResult, ItemNotification, ModelListResult, Skill,
    SkillsListResult, ThreadItem, ThreadNameUpdatedNotification, ThreadStartResult, Turn,
    TurnCompletedNotification, TurnStartResult,
};
use super::transport::{Message, Transport};

/// State the message handler and the command loop both need.
struct Shared {
    /// The wire, so a server request can be answered from either task.
    transport: Arc<Transport>,
    ev_tx: Sender<Event>,
    agent: &'static str,
    /// The session's working directory: which repo skills are in reach.
    cwd: PathBuf,
    permissions: WeakPermissions,
    /// The skills a prompt can invoke as `/name`, as last listed. Refreshed when
    /// Codex says the skill files changed.
    skills: Mutex<Vec<Skill>>,
    /// Names our permission prompts; the wire's request id belongs to the agent.
    next_request_id: AtomicU64,
    /// Fires when the in-flight turn ends, carrying the turn's final state, so
    /// the command loop can stay serial exactly like the ACP backend's
    /// prompt-and-wait.
    turn_done: Mutex<Option<oneshot::Sender<Turn>>>,
    /// Command rows still running, per item. The UI *replaces* a row's content
    /// rather than appending, so deltas have to be summed — and rendered with
    /// their command — before being sent up.
    commands: Mutex<HashMap<String, CommandRow>>,
    /// Rendered content per item, so an approval prompt can show the diff it
    /// refers to — the request itself carries only the item id.
    details: Mutex<HashMap<String, String>>,
    /// The running turn's id, as `turn/started` reports it. An interrupt needs
    /// it, and a compaction's start response does not carry one.
    turn_id: Mutex<Option<String>>,
    /// Agent messages that streamed as deltas this turn. One that completes
    /// without having streamed — a review's findings — is shown whole instead.
    streamed: Mutex<HashSet<String>>,
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

/// What starts a turn. A prompt carries the session's picks; the built-ins that
/// run as turns are methods of their own, which take none of them.
#[derive(Clone, Copy)]
enum TurnRequest<'a> {
    Prompt(&'a str),
    Compact,
    /// `review/start`'s target.
    Review(&'a Value),
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
        // Codex has no mode or access axis; its own sandbox policy is the one
        // boundary it exposes.
        let event = SessionOptionsBuilder::new(agent)
            .models(self.models.clone(), self.model.clone())
            .sandboxes(
                convert::sandbox_options(),
                self.sandbox
                    .clone()
                    .or_else(|| self.reported_sandbox.clone()),
            )
            .reasoning(
                convert::reasoning_options(&self.efforts, self.model.as_deref()),
                self.reasoning.clone(),
            )
            .build();
        emit(ev_tx, event);
    }
}

pub(in crate::backend) async fn run(
    cfg: Config,
    cwd: PathBuf,
    ev_tx: Sender<Event>,
    mut cmd_rx: mpsc::UnboundedReceiver<Cmd>,
    mut cancels: CancelRequests,
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
        agent: cfg.agent,
        cwd: cwd.clone(),
        permissions,
        skills: Mutex::new(Vec::new()),
        next_request_id: AtomicU64::new(1),
        turn_done: Mutex::new(None),
        commands: Mutex::new(HashMap::new()),
        details: Mutex::new(HashMap::new()),
        turn_id: Mutex::new(None),
        streamed: Mutex::new(HashSet::new()),
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
    // A thread that already carries a title reports it here; a freshly started
    // one cannot, since `thread/start` takes no name — its title arrives later
    // as a `thread/name/updated` notification, if the agent generates one.
    if let Some(title) = started
        .thread
        .name
        .as_deref()
        .filter(|title| !title.trim().is_empty())
    {
        emit(
            &ev_tx,
            Event::SessionTitle {
                title: title.to_string(),
            },
        );
    }

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
    refresh_skills(&shared, false).await;

    // Headless convenience, matching the ACP backend: send one prompt before
    // the UI loop starts.
    if let Some(auto) = cfg.auto_prompt.clone() {
        emit(&ev_tx, Event::Log(format!("auto prompt: {auto}")));
        run_prompt(
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
            &mut cancels,
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
                run_prompt(
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
                    &mut cancels,
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

    // Wake anything still waiting on a decision, so the agent is never left
    // hanging on a connection that is going away.
    release_pending_permissions(&shared.permissions);
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

/// Handle one prompt: a built-in command, or a turn.
///
/// app-server parses no slashes, so the built-ins are recognised here and sent
/// as the methods Codex's TUI calls for them (see [`convert::builtin_command`]).
/// Whatever they do, the UI opened a turn for the prompt, so each one settles.
async fn run_prompt(
    transport: &Transport,
    shared: &Shared,
    ev_tx: &Sender<Event>,
    thread_id: &str,
    overrides: TurnOverrides<'_>,
    text: &str,
    cancels: &mut CancelRequests,
) {
    let request = match convert::builtin_command(text) {
        None => TurnRequest::Prompt(text),
        Some(BuiltinCommand::Init) => TurnRequest::Prompt(convert::INIT_PROMPT),
        Some(BuiltinCommand::Compact) => TurnRequest::Compact,
        Some(BuiltinCommand::Review(target)) => {
            let request = TurnRequest::Review(&target);
            return run_turn(
                transport, shared, ev_tx, thread_id, overrides, request, cancels,
            )
            .await;
        }
        Some(BuiltinCommand::Rename(name)) => {
            let params = json!({ "threadId": thread_id, "name": name });
            let failure = match transport.request("thread/name/set", params).await {
                // The agent also says so as `thread/name/updated`; reporting it
                // here does not depend on that echo reaching this connection.
                Ok(_) => {
                    emit(ev_tx, Event::SessionTitle { title: name });
                    None
                }
                Err(err) => Some(format!("重命名失败：{err}")),
            };
            for event in settle("completed", failure) {
                emit(ev_tx, event);
            }
            return;
        }
        Some(BuiltinCommand::Usage(usage)) => {
            for event in settle("completed", Some(usage)) {
                emit(ev_tx, event);
            }
            return;
        }
    };
    run_turn(
        transport, shared, ev_tx, thread_id, overrides, request, cancels,
    )
    .await;
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
    turn: TurnRequest<'_>,
    cancels: &mut CancelRequests,
) {
    forget_stale_cancels(cancels);
    // Installed before the turn starts, so a completion cannot arrive before
    // anyone is listening for it.
    let (done_tx, done_rx) = oneshot::channel();
    *shared.turn_done.lock().unwrap() = Some(done_tx);
    *shared.turn_id.lock().unwrap() = None;
    let started = match turn {
        TurnRequest::Prompt(text) => {
            let input = convert::turn_input(text, &shared.skills.lock().unwrap());
            let mut params = json!({
                "threadId": thread_id,
                "input": input,
            });
            if let Some(model) = overrides.model {
                params["model"] = json!(model);
            }
            // Repeated on every turn, not just on the thread, so a pick made
            // mid-session takes effect on the next turn without reopening
            // anything — the protocol makes a turn-level override the thread's
            // default from then on. An unrecognised value yields nothing,
            // leaving the current policy be.
            if let Some(policy) = overrides.sandbox.and_then(convert::sandbox_policy) {
                params["sandboxPolicy"] = policy;
            }
            // Per turn, and only per turn: `thread/start` takes no effort, so
            // this is the one place a reasoning pick can take effect. The value
            // is an open string advertised by the model, so it is passed through
            // rather than validated — an unknown one is the agent's to reject,
            // not kumu's to guess.
            if let Some(effort) = overrides.reasoning {
                params["effort"] = json!(effort);
            }
            emit(ev_tx, Event::Log("prompt sent".into()));
            request::<TurnStartResult>(transport, "turn/start", params)
                .await
                .map(|started| Some(started.turn.id))
        }
        // The start response is only an acknowledgement; the turn it runs as
        // announces itself with `turn/started`.
        TurnRequest::Compact => {
            let params = json!({ "threadId": thread_id });
            request::<Value>(transport, "thread/compact/start", params)
                .await
                .map(|_| None)
        }
        // Inline, on this thread: a detached review would run on a thread this
        // session does not follow.
        TurnRequest::Review(target) => {
            let params = json!({ "threadId": thread_id, "target": target, "delivery": "inline" });
            request::<TurnStartResult>(transport, "review/start", params)
                .await
                .map(|started| Some(started.turn.id))
        }
    };
    // The turn's id is what an interrupt addresses. A start response that hands
    // one over is the earliest place to learn it; otherwise `turn/started` is.
    let turn_id = match started {
        Ok(turn_id) => {
            if let Some(turn_id) = &turn_id {
                emit(ev_tx, Event::Log(format!("turn started: {turn_id}")));
            }
            turn_id
        }
        Err(err) => {
            // Nothing will complete, so drop the waiter rather than leave the
            // next prompt to inherit it.
            shared.turn_done.lock().unwrap().take();
            emit(ev_tx, Event::Error(err));
            return;
        }
    };

    // The interrupt has to reach the agent while the turn is running, so it is
    // watched for here rather than on the command channel: the loop is parked on
    // this very await.
    tokio::pin!(done_rx);
    let (mut cancel_wanted, mut interrupted) = (false, false);
    let outcome = loop {
        if cancel_wanted && !interrupted {
            let known = turn_id
                .clone()
                .or_else(|| shared.turn_id.lock().unwrap().clone());
            if let Some(turn_id) = known {
                interrupted = true;
                let params = json!({"threadId": thread_id, "turnId": turn_id});
                if let Err(err) = transport.request("turn/interrupt", params).await {
                    emit(ev_tx, Event::Log(format!("turn/interrupt failed: {err}")));
                }
            }
        }
        tokio::select! {
            outcome = &mut done_rx => break outcome,
            _ = cancels.recv(), if !cancel_wanted => {
                cancel_wanted = true;
                emit(ev_tx, Event::Log("cancel requested".into()));
            }
            // A compaction learns its id from `turn/started`, which can trail
            // the request to cancel it; look again shortly rather than drop it.
            _ = tokio::time::sleep(Duration::from_millis(50)), if cancel_wanted && !interrupted => {}
        }
    };

    match outcome {
        Ok(turn) => {
            let failure = (turn.status == "failed").then(|| {
                turn.error
                    .map(|error| error.message)
                    .unwrap_or_else(|| "本轮失败".to_string())
            });
            // `interrupted` is the wire's word for the cancel the user asked
            // for, so the boundary reports it as such rather than passing the
            // agent's spelling through to the UI.
            let stop = if turn.status == "interrupted" {
                CANCELLED.to_string()
            } else {
                turn.status.clone()
            };
            for event in settle(stop, failure) {
                emit(ev_tx, event);
            }
        }
        // The handler went away with the connection.
        Err(_) => emit(ev_tx, Event::Done("closed".into())),
    }
}

/// List the session's skills and report them as its commands.
///
/// Best effort: a failure costs the composer its menu, not the session, so it
/// only reaches the log. `force_reload` bypasses Codex's cache, for when it has
/// said the files changed.
async fn refresh_skills(shared: &Shared, force_reload: bool) {
    let params = json!({ "cwds": [shared.cwd], "forceReload": force_reload });
    let listed = match request::<SkillsListResult>(&shared.transport, "skills/list", params).await {
        Ok(listed) => listed,
        Err(err) => {
            emit(
                &shared.ev_tx,
                Event::Log(format!("skills/list failed: {err}")),
            );
            return;
        }
    };
    let skills = convert::skill_catalog(listed);
    let commands = convert::command_catalog(&skills);
    *shared.skills.lock().unwrap() = skills;
    emit(
        &shared.ev_tx,
        Event::Commands {
            agent: shared.agent,
            commands,
        },
    );
}

/// Fold server messages into events until the connection ends.
async fn handle_messages(shared: Arc<Shared>, mut messages: mpsc::UnboundedReceiver<Message>) {
    while let Some(message) = messages.recv().await {
        match message {
            // Its own task: the reply arrives on this very loop, so awaiting it
            // here would wait on itself.
            Message::Notification { method, .. } if method == "skills/changed" => {
                let shared = shared.clone();
                tokio::spawn(async move { refresh_skills(&shared, true).await });
            }
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
                shared.streamed.lock().unwrap().insert(delta.item_id);
                emit(&shared.ev_tx, Event::Agent(delta.delta));
            }
        }
        // Both the readable summary and raw reasoning become thinking text;
        // which one the model supports is not kumu's business. The item id is
        // the reasoning's label.
        "item/reasoning/summaryTextDelta" | "item/reasoning/textDelta" => {
            if let Ok(delta) = serde_json::from_value::<DeltaNotification>(params) {
                emit(
                    &shared.ev_tx,
                    Event::Thought {
                        text: delta.delta,
                        message: Some(delta.item_id),
                    },
                );
            }
        }
        "item/commandExecution/outputDelta" => {
            let Ok(delta) = serde_json::from_value::<DeltaNotification>(params) else {
                return;
            };
            // The row's content is replaced, not appended to, so the whole
            // accumulated output is what has to be sent.
            let content = {
                let mut commands = shared.commands.lock().unwrap();
                let row = commands.entry(delta.item_id.clone()).or_default();
                row.output.push_str(&delta.delta);
                convert::command_content(&row.command, Some(&row.output))
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
            let done = method == "item/completed";
            if let Some(event) = convert::compaction_event(&item, done) {
                emit(&shared.ev_tx, event);
                return;
            }
            // A message that never streamed is shown whole; one that did has
            // already been shown chunk by chunk.
            if let ThreadItem::AgentMessage { id, text } = &item {
                if done && !text.is_empty() && !shared.streamed.lock().unwrap().contains(id) {
                    emit(&shared.ev_tx, Event::Agent(text.clone()));
                }
                return;
            }
            let detail = {
                let mut commands = shared.commands.lock().unwrap();
                let finished = if method == "item/completed" {
                    // The deltas are dropped rather than left to outlive the
                    // item, but still render it if it carries no output itself.
                    commands.remove(item.id())
                } else {
                    // Output can stream before the item starts; the row keeps
                    // it and learns the command it belongs to.
                    if let ThreadItem::CommandExecution { command, .. } = &item {
                        commands.entry(item.id().to_string()).or_default().command =
                            command.clone();
                    }
                    None
                };
                let streamed = finished
                    .as_ref()
                    .or_else(|| commands.get(item.id()))
                    .map(|row| row.output.as_str());
                convert::item_content(&item, streamed)
            };
            if !detail.is_empty() {
                shared
                    .details
                    .lock()
                    .unwrap()
                    .insert(item.id().to_string(), detail.clone());
            }
            if let Some(event) = convert::tool_event_with_content(&item, detail) {
                emit(&shared.ev_tx, event);
            }
        }
        // The agent titles the thread as the conversation develops. A cleared
        // name (`null`) is not a title, so the one already shown stands rather
        // than being blanked.
        "thread/name/updated" => {
            let Ok(notification) = serde_json::from_value::<ThreadNameUpdatedNotification>(params)
            else {
                return;
            };
            if let Some(title) = notification.thread_name.filter(|t| !t.trim().is_empty()) {
                emit(&shared.ev_tx, Event::SessionTitle { title });
            }
        }
        "turn/completed" => {
            let Ok(notification) = serde_json::from_value::<TurnCompletedNotification>(params)
            else {
                return;
            };
            shared.commands.lock().unwrap().clear();
            shared.streamed.lock().unwrap().clear();
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
        // The id an interrupt addresses, for a turn whose start response did not
        // hand it over.
        "turn/started" => {
            if let Some(id) = params.pointer("/turn/id").and_then(Value::as_str) {
                *shared.turn_id.lock().unwrap() = Some(id.to_string());
            }
        }
        // A compaction's progress is its `contextCompaction` item; this is the
        // deprecated notification Codex still sends alongside it.
        "thread/compacted"
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

/// A command row's state between its output deltas and its completion.
#[derive(Default)]
struct CommandRow {
    command: String,
    output: String,
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

    // Waiting here would stall every notification queued behind it, so the
    // answer is awaited on its own task; the wire id travels with it because
    // the transport needs the agent's id, not ours.
    let transport = shared.transport.clone();
    let ev_tx = shared.ev_tx.clone();
    ask_user(
        table,
        &shared.next_request_id,
        &shared.ev_tx,
        PermissionQuestion {
            tool_call_id: prompt.item_id,
            title: prompt.title,
            detail: prompt.detail,
            options: convert::approval_options(),
        },
        move |option_id| {
            emit(
                &ev_tx,
                Event::Log(format!("permission answered: {option_id:?}")),
            );
            transport.respond(id, decision_payload(option_id.as_deref()));
        },
    );
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

    use super::{Shared, Transport, handle_notification};
    use serde_json::json;
    use std::collections::{HashMap, HashSet};
    use std::sync::Mutex;
    use std::sync::atomic::AtomicU64;
    use std::sync::{Arc, Weak};

    /// A `Shared` wired to nowhere — a detached transport nothing reads, no
    /// permissions table — plus the receiving half of its event channel, for
    /// tests that drive `handle_notification` directly instead of a real
    /// `codex app-server` process. Same shape as the Claude backend's own
    /// synthetic-stream tests.
    fn shared_for_test() -> (Shared, std::sync::mpsc::Receiver<Event>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let shared = Shared {
            transport: Arc::new(Transport::detached_for_test()),
            ev_tx: tx,
            agent: "codex",
            cwd: PathBuf::new(),
            permissions: Weak::new(),
            skills: Mutex::new(Vec::new()),
            next_request_id: AtomicU64::new(1),
            turn_done: Mutex::new(None),
            commands: Mutex::new(HashMap::new()),
            details: Mutex::new(HashMap::new()),
            turn_id: Mutex::new(None),
            streamed: Mutex::new(HashSet::new()),
        };
        (shared, rx)
    }

    /// `Event` carries no `Debug` impl, so a failing assertion names what
    /// actually came through by kind instead.
    fn kinds(events: &[Event]) -> Vec<&'static str> {
        events
            .iter()
            .map(|e| match e {
                Event::Log(_) => "log",
                Event::Agent(_) => "agent",
                Event::Thought { .. } => "thought",
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

    /// A reply streams in as agent-message deltas.
    #[test]
    fn an_agent_message_delta_streams_as_agent_text() {
        let (shared, rx) = shared_for_test();
        handle_notification(
            &shared,
            "item/agentMessage/delta",
            json!({"itemId": "i1", "delta": "ok"}),
        );
        let events: Vec<Event> = rx.try_iter().collect();
        assert!(
            matches!(events.as_slice(), [Event::Agent(text)] if text == "ok"),
            "expected one Agent(\"ok\"): {:?}",
            kinds(&events)
        );
    }

    /// A review's findings arrive as a completed agent message that never
    /// streamed, so they are shown whole; a message that did stream is not
    /// shown twice.
    #[test]
    fn a_message_that_never_streamed_is_shown_on_completion() {
        let (shared, rx) = shared_for_test();
        handle_notification(
            &shared,
            "item/agentMessage/delta",
            json!({"itemId": "streamed", "delta": "ok"}),
        );
        for (id, text) in [("streamed", "ok"), ("review", "未发现问题")] {
            handle_notification(
                &shared,
                "item/completed",
                json!({"item": {"type": "agentMessage", "id": id, "text": text}}),
            );
        }
        let texts: Vec<String> = rx
            .try_iter()
            .filter_map(|event| match event {
                Event::Agent(text) => Some(text),
                _ => None,
            })
            .collect();
        assert_eq!(texts, ["ok", "未发现问题"]);
    }

    /// The UI replaces a tool row's content rather than appending to it, so
    /// consecutive output deltas for the same item have to be summed before
    /// being sent up — each `ToolCall` carries everything accumulated so far,
    /// not just the new chunk.
    #[test]
    fn command_output_deltas_accumulate_before_sending() {
        let (shared, rx) = shared_for_test();
        handle_notification(
            &shared,
            "item/commandExecution/outputDelta",
            json!({"itemId": "cmd1", "delta": "a"}),
        );
        handle_notification(
            &shared,
            "item/commandExecution/outputDelta",
            json!({"itemId": "cmd1", "delta": "b"}),
        );
        let contents: Vec<String> = rx
            .try_iter()
            .filter_map(|e| match e {
                Event::ToolCall { content, .. } => Some(content),
                _ => None,
            })
            .collect();
        assert_eq!(contents, vec!["输出：\na", "输出：\nab"]);
    }

    /// An item starting then completing reports the same call twice: running,
    /// then whatever its final status was — and the accumulated output for it
    /// is dropped once the completed item's own aggregated output supersedes
    /// it.
    #[test]
    fn item_started_then_completed_reports_running_then_its_outcome() {
        let (shared, rx) = shared_for_test();
        handle_notification(
            &shared,
            "item/commandExecution/outputDelta",
            json!({"itemId": "cmd1", "delta": "partial"}),
        );
        handle_notification(
            &shared,
            "item/started",
            json!({"item": {
                "type": "commandExecution", "id": "cmd1", "command": "ls",
                "status": "inProgress",
            }}),
        );
        handle_notification(
            &shared,
            "item/completed",
            json!({"item": {
                "type": "commandExecution", "id": "cmd1", "command": "ls",
                "status": "completed", "aggregatedOutput": "done output",
            }}),
        );
        let statuses: Vec<&str> = rx
            .try_iter()
            .filter_map(|e| match e {
                Event::ToolCall { id, status, .. } if id == "cmd1" => Some(status),
                _ => None,
            })
            .collect();
        assert_eq!(statuses, vec!["", "running", "done"]);
        assert!(
            shared.commands.lock().unwrap().get("cmd1").is_none(),
            "the completed item's own output supersedes the accumulated deltas"
        );
    }

    /// `aggregatedOutput` is nullable, so a completed item without it keeps the
    /// output its deltas streamed rather than shrinking to just the command.
    #[test]
    fn a_completed_command_without_output_keeps_its_streamed_output() {
        let (shared, rx) = shared_for_test();
        handle_notification(
            &shared,
            "item/commandExecution/outputDelta",
            json!({"itemId": "cmd1", "delta": "streamed"}),
        );
        handle_notification(
            &shared,
            "item/completed",
            json!({"item": {
                "type": "commandExecution", "id": "cmd1", "command": "ls",
                "status": "completed", "aggregatedOutput": null,
            }}),
        );
        let last = rx
            .try_iter()
            .filter_map(|e| match e {
                Event::ToolCall { content, .. } => Some(content),
                _ => None,
            })
            .last();
        assert_eq!(last.as_deref(), Some("命令：\nls\n\n输出：\nstreamed"));
        assert!(shared.commands.lock().unwrap().is_empty());
    }

    /// The agent's own title for the thread replaces kumu's; a cleared name
    /// (`null`) is not a title, so nothing is emitted for it.
    #[test]
    fn thread_name_updated_reports_a_title_but_not_a_clearing() {
        let (shared, rx) = shared_for_test();
        handle_notification(
            &shared,
            "thread/name/updated",
            json!({"threadName": "重构计划"}),
        );
        handle_notification(&shared, "thread/name/updated", json!({"threadName": null}));
        let events: Vec<Event> = rx.try_iter().collect();
        assert!(
            matches!(events.as_slice(), [Event::SessionTitle { title }] if title == "重构计划"),
            "expected exactly one title, and not for the clearing: {:?}",
            kinds(&events)
        );
    }

    /// `turn/completed` is what wakes the command loop's wait for the turn —
    /// exactly like a `oneshot` reply — and clears the output accumulated for
    /// it.
    #[test]
    fn turn_completed_wakes_whoever_is_waiting_on_the_turn() {
        let (shared, rx) = shared_for_test();
        handle_notification(
            &shared,
            "item/commandExecution/outputDelta",
            json!({"itemId": "cmd1", "delta": "x"}),
        );
        let (done_tx, mut done_rx) = tokio::sync::oneshot::channel();
        *shared.turn_done.lock().unwrap() = Some(done_tx);
        handle_notification(
            &shared,
            "turn/completed",
            json!({"turn": {"id": "t1", "status": "completed"}}),
        );
        let turn = done_rx.try_recv().expect("turn/completed should wake it");
        assert_eq!(turn.status, "completed");
        assert!(shared.commands.lock().unwrap().is_empty());
        drop(rx);
    }

    /// A notification method this build has never seen still reaches the log
    /// instead of being silently dropped — the canary for app-server growing a
    /// new notification.
    #[test]
    fn an_unrecognized_notification_is_logged_not_dropped() {
        let (shared, rx) = shared_for_test();
        handle_notification(&shared, "thread/something/new", json!({}));
        let events: Vec<Event> = rx.try_iter().collect();
        assert!(
            matches!(&events[..], [Event::Log(msg)] if msg.contains("thread/something/new")),
            "expected a log naming the unrecognized method: {:?}",
            kinds(&events)
        );
    }

    /// End-to-end: spawn a real Codex session, send one prompt, and expect
    /// streamed agent text followed by `Done` — the same path the home window
    /// takes. Ignored by default, so a plain `cargo test` never drives a real
    /// agent; `cargo test -- --ignored` runs it. Two properties of the machine
    /// then count as skips rather than failures: the binary may be absent, and a
    /// Codex that is not signed in says so.
    ///
    /// This is also the proof that a sandbox pick reaches Codex: the session is
    /// told to use `read-only` and then has to report that back, which only the
    /// agent can confirm. A reasoning pick is sent with it and rides on the
    /// turn, so a turn that completes is the agent accepting the effort.
    #[test]
    #[ignore = "drives the real codex binary"]
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
